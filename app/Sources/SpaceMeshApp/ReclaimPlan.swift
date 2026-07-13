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
    /// 화면에 보여줄 대표 위치.
    let path: String
    /// 실제로 휴지통에 보낼 경로들. 보통 `path` 하나지만, 정리 룰은 다른 항목이
    /// 안쪽을 따로 잡고 있으면 그 자식들을 뺀 나머지가 들어온다 —
    /// **실행은 반드시 이걸 써야 한다.** `path`를 지우면 사용자가 따로 고를 수
    /// 있어야 할 항목까지 함께 날아간다 (core의 delete_paths 계약).
    let deletePaths: [String]
    let estimatedBytes: UInt64
    let source: PlanSource
    /// "safe" = 실행 시트에서 기본 체크, "warn" = 기본 해제 (검토 필요).
    let safety: String
    let recreateCommand: String
}

/// 각 탭의 후보 타입 → PlanItem 변환은 여기 한 곳에만 둔다 — 출처(source)와
/// safety 정책이 뷰마다 흩어져 드리프트하지 않게.
extension PlanItem {
    init(_ c: CleanupCandidate) {
        self.init(
            path: c.path, deletePaths: c.deletePaths, estimatedBytes: c.allocatedSize,
            source: .rule, safety: c.safety, recreateCommand: c.recreateCommand)
    }

    init(_ h: CategoryHitInfo) {
        self.init(
            path: h.path, deletePaths: [h.path], estimatedBytes: h.allocatedSize,
            source: .category, safety: h.safety, recreateCommand: h.recreateCommand)
    }

    /// 중복 파일 — 사용자 데이터라 사본이 남더라도 warn으로 담아 시트에서 재확인.
    init(duplicatePath path: String, estimated: UInt64) {
        self.init(
            path: path, deletePaths: [path], estimatedBytes: estimated,
            source: .duplicate, safety: "warn", recreateCommand: "")
    }

    /// 트리맵 대용량 파일 — 정체 미상이라 warn.
    init(_ f: BigFile) {
        self.init(
            path: f.path, deletePaths: [f.path], estimatedBytes: f.allocatedSize,
            source: .bigFile, safety: "warn", recreateCommand: "")
    }
}

/// 실행 결과 — 예상 vs 실측 회수량 (F1의 핵심 신뢰 지표).
struct ReclaimReport {
    let movedCount: Int
    let skippedCount: Int
    let estimated: UInt64
    /// 증분 재스캔으로 측정한 실제 감소량. nil = 측정 불가 — 0("아무것도 못
    /// 비웠다")과 반드시 구별해서 보여준다.
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
        // 같은 경로/조상 관계라도 실행 대상(deletePaths)이 물리적으로 겹치지 않으면
        // 서로 다른 정리 작업이다 — 중첩 룰이 같은 부모 아래 disjoint한 하위집합을
        // 각자 맡는 경우가 그렇다. path만 보고 지우면 그런 항목을 조용히 잃는다.
        // (문자열 Set 동일성이 아니라 포함관계로 봐야 한다 — 부모의 deletePaths가
        // 자손 디렉토리를 통째로 포함하면 실제로는 같은 바이트를 가리킨다.)
        func overlaps(_ a: PlanItem, _ b: PlanItem) -> Bool {
            a.deletePaths.contains { p in
                b.deletePaths.contains { q in
                    isSameOrDescendant(p, of: q) || isSameOrDescendant(q, of: p)
                }
            }
        }
        // 같은 경로나 조상이 이미 담겨 있으면 무시, 자손이 담겨 있으면 자손을 밀어낸다.
        if items.contains(where: { isSameOrDescendant(item.path, of: $0.path) && overlaps(item, $0) })
        {
            return
        }
        items.removeAll {
            isSameOrDescendant($0.path, of: item.path) && $0.path != item.path
                && overlaps(item, $0)
        }
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

    /// selection에 포함된 항목을 휴지통으로 이동하고, 이동한 경로만 증분 재스캔해
    /// 실측 회수량을 채운다.
    func execute(selection: Set<String>, appModel: AppModel) {
        guard !isExecuting else { return }
        let targets = items.filter { selection.contains($0.path) }
        guard !targets.isEmpty else { return }
        isExecuting = true
        report = nil
        message = nil

        // 휴지통 이동 — 즉시 삭제 경로와 같은 공용 엔진(안전 가드 포함).
        // 항목의 deletePaths 중 일부만 이동에 성공할 수 있다(안전가드/실패로
        // 일부 스킵). core는 항목 단위 크기만 주고 경로별 실제 크기는 모르므로,
        // 성공한 경로 수만큼 예상치를 비례 배분하고 나머지 경로는 새 PlanItem으로
        // 잘라 플랜에 그대로 남긴다 — 통째로 지우면 남은 경로를 재시도할 방법이
        // 없어지고, 실측과 무관하게 estimated가 0으로 잡혀 거짓 편차 경고를 낸다.
        var batch: [TrashRecord] = []
        var skipped = 0
        var estimated: UInt64 = 0
        var remaining: [PlanItem] = []
        let targetPaths = Set(targets.map(\.path))
        for item in targets {
            let sized = item.deletePaths.map { (path: $0, size: UInt64(0)) }
            let (moved, skip) = moveToTrash(paths: sized)
            batch.append(contentsOf: moved)
            skipped += skip

            let movedSet = Set(moved.map(\.original))
            let leftover = item.deletePaths.filter { !movedSet.contains($0) }
            if leftover.isEmpty {
                estimated += item.estimatedBytes
            } else if movedSet.isEmpty {
                remaining.append(item)
            } else {
                let movedShare =
                    item.estimatedBytes * UInt64(movedSet.count) / UInt64(item.deletePaths.count)
                estimated += movedShare
                remaining.append(
                    PlanItem(
                        path: item.path, deletePaths: leftover,
                        estimatedBytes: item.estimatedBytes - movedShare,
                        source: item.source, safety: item.safety,
                        recreateCommand: item.recreateCommand))
            }
        }
        lastBatch = batch
        items.removeAll { targetPaths.contains($0.path) }
        items.append(contentsOf: remaining)

        let rootPath = appModel.scannedRoot.isEmpty ? NSHomeDirectory() : appModel.scannedRoot
        lastLogId = batch.isEmpty
            ? nil
            : try? reclaimLogAdd(
                dbPath: AppModel.dbPath, rootPath: rootPath,
                itemCount: UInt64(batch.count), estimated: estimated)

        let skippedCount = skipped
        let logId = lastLogId
        verify(itemPaths: batch.map(\.original), appModel: appModel) { measured in
            if let logId, let measured {
                try? reclaimLogSetMeasured(dbPath: AppModel.dbPath, id: logId, measured: measured)
            }
            if batch.isEmpty {
                self.message = "이동한 항목이 없습니다 (\(skippedCount)개 보호됨/실패)"
                self.report = nil
            } else {
                self.report = ReclaimReport(
                    movedCount: batch.count, skippedCount: skippedCount,
                    estimated: estimated, measured: measured)
            }
            self.isExecuting = false
        }
    }

    /// 마지막 배치를 휴지통에서 복원하고 트리를 다시 실측한다.
    ///
    /// execute()의 검증 재스캔이 도는 동안(isExecuting=true) 되돌리기를 누르면
    /// 두 verify() 콜백이 경합해 같은 reclaim_log 레코드를 서로 다른 값으로
    /// 덮어쓴다 — execute()와 같은 가드를 공유한다(트레이 버튼도 disabled).
    func undoLastBatch(appModel: AppModel) {
        guard !isExecuting else { return }
        isExecuting = true
        let restored = restoreFromTrash(lastBatch)
        if let logId = lastLogId {
            try? reclaimLogSetUndone(dbPath: AppModel.dbPath, id: logId)
        }
        let restoredPaths = lastBatch.map(\.original)
        lastBatch = []
        report = nil
        message = "복원: \(restored)개"
        verify(itemPaths: restoredPaths, appModel: appModel) { _ in
            self.isExecuting = false
        }
    }

    /// 이동/복원된 경로를 증분 재스캔해 루트 집계의 변화를 실측치로 넘긴다.
    ///
    /// 경로를 그대로 넘기는 이유: 삭제가 "노드 제거"로 관측되고, ~/.Trash는
    /// 대상이 아니라 다시 훑지 않는다 — 휴지통으로 옮긴 바이트가 트리에
    /// 재계상되지 않는다.
    ///
    /// 측정 불가(nil) 조건 — 가짜 '실측 0'을 만들지 않기 위해:
    /// - 핸들이 없거나 재스캔 자체가 실패
    /// - **풀스캔으로 강등된 경우**. 강등되면 ~/.Trash까지 다시 훑어 방금 옮긴
    ///   바이트가 그대로 재계상되고, 실측치가 0에 가깝게 상쇄된다. core가
    ///   degraded 플래그로 알려주므로 그때는 측정을 포기한다.
    ///   (강등 사유: 루트 밖 경로, 루트 직속 항목, 하드링크 소유권 이탈 등)
    private func verify(
        itemPaths paths: [String], appModel: AppModel,
        completion: @escaping @MainActor (Int64?) -> Void
    ) {
        guard let handle = appModel.handle, !paths.isEmpty else {
            completion(nil)
            return
        }
        let before = appModel.rootAllocated
        Task {
            // dbPath를 비워 스냅샷/커서를 건드리지 않는다 — 스냅샷 영속화는
            // BackgroundAgent의 몫이고, 여기서 cursor 0을 쓰면 증분 재개가 깨진다.
            let report = try? await Task.detached(priority: .userInitiated) {
                try handle.rescanPaths(
                    paths: paths, minFileMib: AppModel.scanRecordMinFileMib,
                    dbPath: "", fseventCursor: 0)
            }.value
            guard let report else {
                completion(nil)
                return
            }
            // 강등이든 아니든 새 핸들의 트리는 항상 올바르다 — 받아들인다.
            appModel.adopt(handle: report.handle)
            guard !report.degraded else {
                completion(nil)
                return
            }
            // 줄어든 만큼이 회수량(양수). 늘었다면 음수로 그대로 보고한다.
            let after = appModel.rootAllocated
            completion(Int64(clamping: before) - Int64(clamping: after))
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
                .disabled(plan.isExecuting)
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
                Text(
                    measured >= 0
                        ? humanBytes(UInt64(measured)) : "-\(humanBytes(UInt64(-measured)))"
                )
                .font(.dataCell)
                .foregroundStyle(deviates(report) ? Theme.warn : Theme.safe)
                .help(
                    deviates(report)
                        ? "예상과의 차이는 대개 APFS 로컬 스냅샷·휴지통 보류분(purgeable) 때문입니다 — 툴바의 PURGEABLE 게이지를 확인하세요"
                        : "실행 직후 영향받은 디렉토리만 다시 스캔해 측정한 값입니다"
                )
            } else {
                InstrumentLabel(text: "실측 불가")
                    .help("증분 재스캔이 풀스캔으로 강등돼 휴지통 보류분과 구분할 수 없었습니다")
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
                NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: item.path)])
            }
        }
    }
}
