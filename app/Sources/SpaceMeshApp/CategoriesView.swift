import AppKit
import SpaceMeshCore
import SwiftUI

/// 잘 알려진 산출물 카테고리(node_modules, cargo target 등)별 그룹 뷰.
/// 스캔된 트리를 재사용하므로 즉시 표시된다. 각 인스턴스에서 "무슨 일이 일어나는지"
/// 확인할 수 있게 프로젝트 경로 + 마커 검증 배지를 함께 보여준다.
struct CategoriesView: View {
    @EnvironmentObject var appModel: AppModel
    @ObservedObject var model: CleanupModel
    let scanTarget: String

    @State private var hits: [CategoryHitInfo] = []
    @State private var selected: Set<String> = []
    @State private var confirmTrash = false
    @State private var loadedForPath: String = ""
    @State private var isLoading = false

    private var grouped: [(id: String, title: String, safety: String, description: String, items: [CategoryHitInfo])] {
        var order: [String] = []
        var buckets: [String: [CategoryHitInfo]] = [:]
        for hit in hits {
            if buckets[hit.categoryId] == nil { order.append(hit.categoryId) }
            buckets[hit.categoryId, default: []].append(hit)
        }
        // 카테고리 합계 내림차순.
        return order
            .map { id -> (String, String, String, String, [CategoryHitInfo]) in
                let items = buckets[id] ?? []
                return (id, items[0].title, items[0].safety, items[0].description, items)
            }
            .sorted {
                $0.4.reduce(UInt64(0)) { $0 + $1.allocatedSize }
                    > $1.4.reduce(UInt64(0)) { $0 + $1.allocatedSize }
            }
            .map { (id: $0.0, title: $0.1, safety: $0.2, description: $0.3, items: $0.4) }
    }

    private var selectedItems: [(path: String, size: UInt64)] {
        hits.filter { selected.contains($0.path) }.map { ($0.path, $0.allocatedSize) }
    }
    private var selectedSize: UInt64 { selectedItems.reduce(0) { $0 + $1.size } }
    private var totalSize: UInt64 { hits.reduce(0) { $0 + $1.allocatedSize } }

    var body: some View {
        VStack(spacing: 0) {
            if appModel.handle == nil {
                VStack(spacing: 12) {
                    Image(systemName: "hammer.circle")
                        .font(.system(size: 40))
                        .foregroundStyle(.secondary)
                    Text("빌드 산출물(node_modules, cargo target 등) 분석은 스캔 결과를 사용합니다")
                        .foregroundStyle(.secondary)
                    Button("'\((scanTarget as NSString).lastPathComponent)' 스캔") {
                        appModel.startScan(path: scanTarget)
                    }
                    .buttonStyle(.borderedProminent)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if appModel.isScanning {
                ScanningView(startedAt: appModel.scanStartedAt, label: "스캔 중")
            } else if isLoading {
                // categories()는 스캔 트리를 순회하며 마커를 검증한다. 느린 머신에선
                // 수 초가 걸려 스피너 없이는 "산출물 없음" 빈 상태처럼 보여 행으로 오인된다.
                VStack(spacing: 12) {
                    ProgressView()
                        .controlSize(.large)
                    InstrumentLabel(text: "빌드 산출물 분석 중")
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if hits.isEmpty {
                VStack(spacing: 10) {
                    Image(systemName: "checkmark.seal")
                        .font(.system(size: 40))
                        .foregroundStyle(.secondary)
                    Text("스캔 범위에 알려진 빌드 산출물이 없습니다").foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                summaryHeader
                Divider()
                categoryList
            }
            Divider()
            CartBar(
                selectedCount: selectedItems.count,
                selectedSize: selectedSize,
                message: model.message,
                undoAvailable: !model.lastBatch.isEmpty,
                onTrash: { confirmTrash = true },
                onUndo: { model.undoLastBatch() },
                onRefresh: { reload() }
            )
        }
        .onAppear { reloadIfNeeded() }
        .onChange(of: appModel.scanSeconds) { reloadIfNeeded(force: true) }
        .confirmationDialog(
            "\(selectedItems.count)개 항목 (\(humanBytes(selectedSize)))을 휴지통으로 이동할까요?",
            isPresented: $confirmTrash, titleVisibility: .visible
        ) {
            Button("휴지통으로 이동", role: .destructive) {
                model.trash(paths: selectedItems)
                // 트리 재스캔 없이 목록에서 제거 (스냅샷은 다음 스캔에서 갱신).
                let moved = Set(selectedItems.map(\.path))
                hits.removeAll { moved.contains($0.path) }
                selected = []
            }
        } message: {
            Text("모두 재생성 가능한 산출물입니다. 휴지통에서 복원할 수도 있습니다.")
        }
    }

    private func reloadIfNeeded(force: Bool = false) {
        guard appModel.handle != nil else { return }
        if force || loadedForPath != appModel.scannedRoot || hits.isEmpty {
            reload()
        }
    }

    private func reload() {
        guard let handle = appModel.handle else { return }
        selected = []
        isLoading = true
        Task {
            let found = await Task.detached { handle.categories() }.value
            self.hits = found
            self.loadedForPath = appModel.scannedRoot
            self.isLoading = false
        }
    }

    private var summaryHeader: some View {
        HStack(spacing: 10) {
            InstrumentLabel(text: "\(grouped.count)개 카테고리")
            InstrumentLabel(text: "\(hits.count)개 위치")
            Text(humanBytes(totalSize))
                .font(.dataCell)
                .foregroundStyle(Theme.accent)
            Spacer()
            Button("유휴 프로젝트만 선택 (6개월+)") {
                selected = Set(
                    hits.filter { $0.verified && ($0.idleDays ?? 0) >= 180 }.map(\.path))
            }
            .font(.system(size: 11, weight: .semibold))
            .buttonStyle(.plain)
            .foregroundStyle(Theme.textDim)
            Button("검증된 항목 모두 선택") {
                selected = Set(hits.filter(\.verified).map(\.path))
            }
            .font(.system(size: 11, weight: .semibold))
            .buttonStyle(.plain)
            .foregroundStyle(Theme.accent)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .background(Theme.panel)
    }

    private var categoryList: some View {
        List {
            ForEach(grouped, id: \.id) { group in
                Section {
                    ForEach(group.items, id: \.path) { hit in
                        hitRow(hit)
                            .listRowBackground(Theme.bg)
                    }
                } header: {
                    categoryHeader(group)
                }
            }
        }
        .listStyle(.inset)
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
    }

    /// 카테고리 구분 헤더 — 큰 제목 + 이 파일이 무엇인지에 대한 설명.
    private func categoryHeader(
        _ group: (id: String, title: String, safety: String, description: String, items: [CategoryHitInfo])
    ) -> some View {
        let groupSize = group.items.reduce(UInt64(0)) { $0 + $1.allocatedSize }
        return VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Image(systemName: Self.icon(for: group.id))
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(Theme.accent)
                    .frame(width: 24)
                Text(group.title)
                    .font(.system(size: 17, weight: .bold))
                    .foregroundStyle(Theme.text)
                Text("\(group.items.count)곳")
                    .font(.dataCell)
                    .foregroundStyle(Theme.textDim)
                Text(humanBytes(groupSize))
                    .font(.dataCell)
                    .foregroundStyle(Theme.accent)
                if group.safety == "warn" {
                    TagBadge(text: "검토 필요", color: Theme.warn)
                }
                Spacer()
                Button("전체 선택") {
                    for item in group.items { selected.insert(item.path) }
                }
                .font(.system(size: 11, weight: .semibold))
                .buttonStyle(.plain)
                .foregroundStyle(Theme.accent)
            }
            Text(group.description)
                .font(.system(size: 12))
                .foregroundStyle(Theme.textDim)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.leading, 32)
            if let first = group.items.first, !first.recreateCommand.isEmpty {
                HStack(spacing: 6) {
                    InstrumentLabel(text: "복원")
                    Text(first.recreateCommand)
                        .font(.pathCell)
                        .foregroundStyle(Theme.textDim)
                        .textSelection(.enabled)
                    Button {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(first.recreateCommand, forType: .string)
                    } label: {
                        Image(systemName: "doc.on.doc")
                            .font(.system(size: 9))
                            .foregroundStyle(Theme.textFaint)
                    }
                    .buttonStyle(.plain)
                    .help("복원 명령 복사")
                    costBadge(first.recreateCost)
                }
                .padding(.leading, 32)
            }
        }
        .textCase(nil)
        .padding(.vertical, 8)
    }

    private func costBadge(_ cost: String) -> some View {
        let (label, color): (String, Color) = switch cost {
        case "high": ("재생성 비용 높음", Theme.warn)
        case "medium": ("재생성 중간", Theme.info)
        default: ("자동 재생성", Theme.safe)
        }
        return TagBadge(text: label, color: color)
    }

    /// 카테고리별 대표 아이콘.
    static func icon(for categoryId: String) -> String {
        switch categoryId {
        case "node-modules", "turbo-cache", "next-build": return "shippingbox.fill"
        case "cargo-target": return "gearshape.2.fill"
        case "python-venv", "pycache", "python-tool-cache": return "chevron.left.forwardslash.chevron.right"
        case "gradle-build", "gradle-project-cache": return "hammer.fill"
        case "cocoapods": return "cube.box.fill"
        case "terraform": return "cloud.fill"
        case "go-vendor": return "folder.fill.badge.gearshape"
        default: return "folder.fill"
        }
    }

    private func hitRow(_ hit: CategoryHitInfo) -> some View {
        HStack(spacing: 10) {
            Toggle(
                "",
                isOn: Binding(
                    get: { selected.contains(hit.path) },
                    set: { on in
                        if on { selected.insert(hit.path) } else { selected.remove(hit.path) }
                    }
                )
            )
            .labelsHidden()
            VStack(alignment: .leading, spacing: 1) {
                Text((hit.projectPath as NSString).lastPathComponent)
                    .font(.system(size: 12.5, weight: .semibold))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                Text(hit.path)
                    .font(.pathCell)
                    .foregroundStyle(Theme.textFaint)
                    .lineLimit(1)
            }
            if let idle = hit.idleDays, idle >= 180 {
                TagBadge(text: "유휴 \(idle / 30)개월", color: Theme.info, icon: "moon.zzz.fill")
                    .help("이 프로젝트의 git 마지막 커밋이 \(idle)일 전입니다")
            }
            if hit.verified {
                TagBadge(text: "확인됨", color: Theme.safe, icon: "checkmark.seal.fill")
                    .help("프로젝트 마커로 정체가 확인되었습니다")
            } else {
                TagBadge(text: "미확인", color: Theme.textFaint)
                    .help("마커 파일이 없어 이름으로만 매칭되었습니다 — 확인 후 삭제하세요")
            }
            Spacer()
            VStack(alignment: .trailing, spacing: 1) {
                Text(humanBytes(hit.allocatedSize))
                    .font(.dataCell)
                    .foregroundStyle(Theme.text)
                Text("\(hit.fileCount.formatted()) files")
                    .font(.pathCell)
                    .foregroundStyle(Theme.textFaint)
            }
        }
        .contextMenu {
            Button("Finder에서 보기") {
                NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: hit.path)])
            }
            Button("프로젝트 폴더 열기") {
                NSWorkspace.shared.activateFileViewerSelecting(
                    [URL(fileURLWithPath: hit.projectPath)])
            }
        }
    }
}
