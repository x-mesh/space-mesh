import AppKit
import Foundation
import SpaceMeshCore
import SwiftUI

/// 회수 플랜 항목의 출처 탭.
enum PlanSource: String {
    case category = "빌드 산출물"
    case rule = "정리"
    case duplicate = "중복"
    case bigFile = "대용량 파일"
}

/// 통합 회수 플랜에 담긴 항목 하나.
struct PlanItem: Identifiable, Equatable {
    var id: String { path }
    let path: String
    let estimatedBytes: UInt64
    let source: PlanSource
    /// "safe" = 실행 시트에서 기본 체크, "warn" = 기본 해제 (검토 필요).
    let safety: String
    let recreateCommand: String
}

/// 실행 결과 — 예상 vs 실측 회수량 (F1의 핵심 신뢰 지표).
struct ReclaimReport {
    let movedCount: Int
    let skippedCount: Int
    let estimated: UInt64
    /// 증분 재스캔으로 측정한 실제 감소량. nil = 측정 불가(핸들 없음 등).
    let measured: Int64?
}

/// 탭에 흩어진 정리 후보를 하나로 모으는 장바구니 (F1).
/// 실행은 휴지통 경유 + undo, 실행 직후 영향 서브트리만 증분 재스캔해
/// 실측 회수량을 리포트하고 reclaim_log에 남긴다.
@MainActor
final class ReclaimPlan: ObservableObject {
    @Published private(set) var items: [PlanItem] = []
    @Published var lastBatch: [TrashRecord] = []
    @Published var isExecuting = false
    @Published var report: ReclaimReport?
    @Published var message: String?
    private var lastLogId: Int64?

    var totalEstimated: UInt64 { items.reduce(0) { $0 + $1.estimatedBytes } }

    // MARK: - 담기 (조상/자손 이중 계산 방지 — CleanupModel과 동일 규칙)

    func add(_ newItems: [PlanItem]) {
        for item in newItems { add(item) }
    }

    func add(_ item: PlanItem) {
        // 같은 경로나 조상이 이미 담겨 있으면 무시, 자손이 담겨 있으면 자손을 밀어낸다.
        if items.contains(where: { $0.path == item.path || item.path.hasPrefix($0.path + "/") }) {
            return
        }
        items.removeAll { $0.path.hasPrefix(item.path + "/") }
        items.append(item)
    }

    func remove(_ item: PlanItem) {
        items.removeAll { $0.path == item.path }
    }

    func clear() {
        items = []
        report = nil
        message = nil
    }

    // MARK: - 실행 + 검증

    /// selection에 포함된 항목을 휴지통으로 이동하고, 이동한 항목의 부모
    /// 디렉토리만 증분 재스캔해 실측 회수량을 채운다.
    func execute(selection: Set<String>, appModel: AppModel) {
        guard !isExecuting else { return }
        let targets = items.filter { selection.contains($0.path) }
        guard !targets.isEmpty else { return }
        isExecuting = true
        report = nil
        message = nil

        let estimated = targets.reduce(UInt64(0)) { $0 + $1.estimatedBytes }
        let rootPath = appModel.scannedRoot.isEmpty ? NSHomeDirectory() : appModel.scannedRoot
        lastLogId = try? reclaimLogAdd(
            dbPath: AppModel.dbPath, rootPath: rootPath,
            itemCount: UInt64(targets.count), estimated: estimated)

        // 휴지통 이동 — 안전 가드는 즉시 삭제 경로와 동일하게 적용.
        var batch: [TrashRecord] = []
        var skipped = 0
        let fm = FileManager.default
        for item in targets {
            guard CleanupModel.isSafeToTrash(item.path) else {
                skipped += 1
                continue
            }
            var resultURL: NSURL?
            do {
                try fm.trashItem(
                    at: URL(fileURLWithPath: item.path), resultingItemURL: &resultURL)
                if let trashURL = resultURL as URL? {
                    batch.append(
                        TrashRecord(
                            original: item.path, trashURL: trashURL, size: item.estimatedBytes))
                }
            } catch {
                skipped += 1
            }
        }
        lastBatch = batch
        let movedPaths = Set(batch.map(\.original))
        items.removeAll { movedPaths.contains($0.path) }

        let skippedCount = skipped
        let logId = lastLogId
        verify(parentsOf: batch.map(\.original), appModel: appModel) { measured in
            if let logId, let measured {
                try? reclaimLogSetMeasured(
                    dbPath: AppModel.dbPath, id: logId, measured: measured)
            }
            self.report = ReclaimReport(
                movedCount: batch.count, skippedCount: skippedCount,
                estimated: estimated, measured: measured)
            if batch.isEmpty {
                self.message = "이동한 항목이 없습니다 (\(skippedCount)개 보호됨/실패)"
            }
            self.isExecuting = false
        }
    }

    /// 마지막 배치를 휴지통에서 복원하고 트리를 다시 실측한다.
    func undoLastBatch(appModel: AppModel) {
        let fm = FileManager.default
        var restored = 0
        for record in lastBatch.reversed() {
            do {
                try fm.moveItem(
                    at: record.trashURL, to: URL(fileURLWithPath: record.original))
                restored += 1
            } catch {
                // 이미 휴지통이 비워졌거나 원위치에 새 항목이 생긴 경우.
            }
        }
        if let logId = lastLogId {
            try? reclaimLogSetUndone(dbPath: AppModel.dbPath, id: logId)
        }
        let parents = lastBatch.map(\.original)
        lastBatch = []
        report = nil
        message = "복원: \(restored)개"
        verify(parentsOf: parents, appModel: appModel) { _ in }
    }

    /// 영향받은 부모 디렉토리만 증분 재스캔해 실측치를 콜백으로 넘긴다.
    /// 핸들이 없거나(스캔 전) 경로가 비면 nil로 즉시 완료.
    private func verify(
        parentsOf paths: [String], appModel: AppModel,
        completion: @escaping @MainActor (Int64?) -> Void
    ) {
        let parents = Array(Set(paths.map { ($0 as NSString).deletingLastPathComponent }))
        guard let handle = appModel.handle, !parents.isEmpty else {
            completion(nil)
            return
        }
        Task {
            let summary = try? await Task.detached(priority: .userInitiated) {
                try handle.refreshPaths(absPaths: parents, minFileMib: 50)
            }.value
            appModel.reload()
            // 음수 delta = 트리가 줄었다 = 회수됨. 측정값은 회수량(양수)으로 뒤집는다.
            completion(summary.map { -$0.deltaAllocated })
        }
    }
}

// MARK: - 트레이 (전 탭 공통 하단 고정)

/// 담긴 항목 수·예상 회수량 적산계 + 실행/비우기. 실행 후에는
/// "예상 vs 실측" 리포트를 같은 자리에서 보여준다.
struct ReclaimTrayView: View {
    @ObservedObject var plan: ReclaimPlan
    @EnvironmentObject var appModel: AppModel
    @State private var showSheet = false

    var body: some View {
        HStack(spacing: 14) {
            InstrumentLabel(text: "회수 플랜")
            if !plan.items.isEmpty {
                Text("\(plan.items.count)")
                    .font(.dataCell)
                    .foregroundStyle(Theme.text)
                InstrumentLabel(text: "예상 회수")
                Text(humanBytes(plan.totalEstimated))
                    .font(.dataCell)
                    .foregroundStyle(Theme.accent)
            }
            if let report = plan.report {
                reportReadout(report)
            } else if let message = plan.message {
                Text(message)
                    .font(.system(size: 12))
                    .monospacedDigit()
                    .foregroundStyle(Theme.textDim)
            }
            Spacer()
            if !plan.lastBatch.isEmpty {
                Button {
                    plan.undoLastBatch(appModel: appModel)
                } label: {
                    Text("되돌리기")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(Theme.textDim)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 5)
                        .overlay(
                            RoundedRectangle(cornerRadius: 6).stroke(Theme.border, lineWidth: 1)
                        )
                }
                .buttonStyle(.plain)
            }
            if !plan.items.isEmpty {
                Button {
                    plan.clear()
                } label: {
                    Text("비우기")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(Theme.textDim)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 5)
                }
                .buttonStyle(.plain)
                Button {
                    showSheet = true
                } label: {
                    Text("검토 후 실행")
                        .font(.system(size: 11, weight: .bold))
                        .tracking(0.6)
                        .foregroundStyle(plan.isExecuting ? Theme.textFaint : Theme.bg)
                        .padding(.horizontal, 14)
                        .padding(.vertical, 6)
                        .background(
                            plan.isExecuting ? Theme.raised : Theme.accent,
                            in: RoundedRectangle(cornerRadius: 6)
                        )
                }
                .buttonStyle(.plain)
                .disabled(plan.isExecuting)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(Theme.panel)
        .sheet(isPresented: $showSheet) {
            ReclaimSheetView(plan: plan)
                .environmentObject(appModel)
        }
    }

    /// 예상 vs 실측 리드아웃. 오차 10% 초과면 조용히 주의색으로 표시.
    private func reportReadout(_ report: ReclaimReport) -> some View {
        HStack(spacing: 8) {
            InstrumentLabel(text: "이동 \(report.movedCount)")
            if report.skippedCount > 0 {
                InstrumentLabel(text: "보호 \(report.skippedCount)")
            }
            InstrumentLabel(text: "예상")
            Text(humanBytes(report.estimated))
                .font(.dataCell)
                .foregroundStyle(Theme.textDim)
            if let measured = report.measured {
                InstrumentLabel(text: "실측")
                Text(measured >= 0 ? humanBytes(UInt64(measured)) : "-\(humanBytes(UInt64(-measured)))")
                    .font(.dataCell)
                    .foregroundStyle(deviates(report) ? Theme.warn : Theme.safe)
                    .help(
                        deviates(report)
                            ? "예상과의 차이는 대개 APFS 로컬 스냅샷·휴지통 보류분(purgeable) 때문입니다 — 툴바의 PURGEABLE 게이지를 확인하세요"
                            : "실행 직후 영향받은 디렉토리만 다시 스캔해 측정한 값입니다"
                    )
            } else {
                InstrumentLabel(text: "실측 불가")
            }
        }
    }

    private func deviates(_ report: ReclaimReport) -> Bool {
        guard let measured = report.measured, report.estimated > 0 else { return false }
        let diff = abs(Double(measured) - Double(report.estimated))
        return diff / Double(report.estimated) > 0.10
    }
}

// MARK: - 실행 전 검토 시트

/// 항목별 safety 검토 시트 — safe는 기본 체크, warn은 기본 해제.
/// 파괴적 행위일수록 조용하고 명확하게 (디자인 원칙 4).
struct ReclaimSheetView: View {
    @ObservedObject var plan: ReclaimPlan
    @EnvironmentObject var appModel: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var selection: Set<String> = []

    private var selectedEstimated: UInt64 {
        plan.items.filter { selection.contains($0.path) }
            .reduce(0) { $0 + $1.estimatedBytes }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("회수 플랜 검토")
                    .font(.system(size: 14, weight: .bold))
                    .foregroundStyle(Theme.text)
                Spacer()
                InstrumentLabel(text: "선택 \(selection.count)/\(plan.items.count)")
                Text(humanBytes(selectedEstimated))
                    .font(.dataCell)
                    .foregroundStyle(Theme.accent)
            }
            .padding(12)
            Divider()
            List {
                ForEach(plan.items) { item in
                    itemRow(item)
                        .listRowBackground(Theme.bg)
                }
            }
            .listStyle(.inset)
            .scrollContentBackground(.hidden)
            .background(Theme.bg)
            Divider()
            HStack(spacing: 10) {
                Text("휴지통으로 이동합니다 — 실행 후 되돌리기와 휴지통 복원이 가능합니다")
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.textFaint)
                Spacer()
                Button("취소") { dismiss() }
                    .buttonStyle(.plain)
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(Theme.textDim)
                Button {
                    plan.execute(selection: selection, appModel: appModel)
                    dismiss()
                } label: {
                    HStack(spacing: 5) {
                        Image(systemName: "trash")
                            .font(.system(size: 10, weight: .bold))
                        Text("\(selection.count)개 휴지통으로 이동")
                            .font(.system(size: 11, weight: .bold))
                    }
                    .foregroundStyle(selection.isEmpty ? Theme.textFaint : Theme.bg)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 6)
                    .background(
                        selection.isEmpty ? Theme.raised : Theme.accent,
                        in: RoundedRectangle(cornerRadius: 6)
                    )
                }
                .buttonStyle(.plain)
                .disabled(selection.isEmpty)
            }
            .padding(12)
        }
        .frame(width: 560, height: 420)
        .background(Theme.bg)
        .onAppear {
            // 안전 항목만 기본 선택 — warn은 사용자가 명시적으로 켠다.
            selection = Set(plan.items.filter { $0.safety == "safe" }.map(\.path))
        }
    }

    private func itemRow(_ item: PlanItem) -> some View {
        HStack(spacing: 10) {
            Toggle(
                "",
                isOn: Binding(
                    get: { selection.contains(item.path) },
                    set: { on in
                        if on { selection.insert(item.path) } else { selection.remove(item.path) }
                    }
                )
            )
            .labelsHidden()
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 6) {
                    Text((item.path as NSString).lastPathComponent)
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.text)
                        .lineLimit(1)
                    TagBadge(text: item.source.rawValue, color: Theme.info)
                    if item.safety == "warn" {
                        TagBadge(text: "검토 필요", color: Theme.warn)
                    }
                }
                Text(item.path)
                    .font(.pathCell)
                    .foregroundStyle(Theme.textFaint)
                    .lineLimit(1)
                if !item.recreateCommand.isEmpty {
                    Text("복원: \(item.recreateCommand)")
                        .font(.pathCell)
                        .foregroundStyle(Theme.textFaint)
                        .lineLimit(1)
                }
            }
            Spacer()
            Text(humanBytes(item.estimatedBytes))
                .font(.dataCell)
                .foregroundStyle(Theme.text)
            Button {
                plan.remove(item)
                selection.remove(item.path)
            } label: {
                Image(systemName: "xmark")
                    .font(.system(size: 9, weight: .bold))
                    .foregroundStyle(Theme.textFaint)
            }
            .buttonStyle(.plain)
            .help("플랜에서 제외")
        }
        .contextMenu {
            Button("Finder에서 보기") {
                NSWorkspace.shared.activateFileViewerSelecting(
                    [URL(fileURLWithPath: item.path)])
            }
        }
    }
}
