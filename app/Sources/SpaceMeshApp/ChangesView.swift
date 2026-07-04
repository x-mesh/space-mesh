import AppKit
import SpaceMeshCore
import SwiftUI

/// 스냅샷 diff — "지난 스캔 이후 무엇이 커졌나".
/// 스캔할 때마다 스냅샷이 자동 축적되고, 여기서 최근 두 개(또는 선택한 기준)를 비교한다.
struct ChangesView: View {
    @EnvironmentObject var appModel: AppModel
    let scanTarget: String

    enum DisplayMode {
        case list, treemap
    }

    @State private var snapshots: [SnapshotInfo] = []
    @State private var entries: [DiffEntryInfo] = []
    @State private var oldID: Int64?
    @State private var minDeltaMib: UInt64 = 10
    @State private var isLoading = false
    @State private var loadError: String?
    @State private var displayMode: DisplayMode = .list
    @State private var diffHandle: DiffHandle?
    /// nil이면 범인 목록, 값이 있으면 해당 경로(루트 아래 세그먼트)를 탐색 중.
    @State private var drillPath: [String]?
    @State private var drillChildren: [DiffChildInfo] = []

    private var netDelta: Int64 { entries.reduce(0) { $0 + $1.delta } }
    private var maxAbsDelta: Int64 { entries.map { abs($0.delta) }.max() ?? 1 }

    /// 변화량의 색: 방향 = 색상(코퍼/틸), 상대 변화율 = 불투명도.
    /// 절대량은 바 길이/타일 면적이 담당하므로 색은 "얼마나 급격한가"를 맡는다.
    static func deltaColor(
        delta: Int64, before: UInt64, opacityRange: ClosedRange<Double>
    ) -> Color {
        let base = delta >= 0 ? Theme.deltaGrow : Theme.deltaShrink
        let ratio = Double(abs(delta)) / Double(max(before, 1))
        let t = min(1.0, ratio.squareRoot()) // 지각적 분포 보정
        let opacity = opacityRange.lowerBound
            + (opacityRange.upperBound - opacityRange.lowerBound) * t
        return base.opacity(opacity)
    }

    /// "rootname/a/b" 형태의 diff 경로 → 루트 아래 세그먼트 ["a","b"].
    private func segments(fromDiffPath path: String) -> [String] {
        path.split(separator: "/").dropFirst().map(String.init)
    }

    private func enterDrill(_ path: [String]) {
        guard let handle = diffHandle else { return }
        drillPath = path
        drillChildren = handle.children(path: path)
    }

    var body: some View {
        VStack(spacing: 0) {
            controlBar
            Rectangle().fill(Theme.border).frame(height: 1)
            if isLoading {
                ProgressView()
                    .controlSize(.small)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if snapshots.count < 2 {
                VStack(spacing: 10) {
                    Image(systemName: "clock.arrow.circlepath")
                        .font(.system(size: 40, weight: .light))
                        .foregroundStyle(Theme.textFaint)
                    Text("비교하려면 스냅샷이 2개 이상 필요합니다")
                        .foregroundStyle(Theme.text)
                    Text("스캔할 때마다 스냅샷이 자동 저장됩니다 — 시간을 두고 다시 스캔해 보세요 (현재 \(snapshots.count)개)")
                        .font(.callout)
                        .foregroundStyle(Theme.textDim)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if entries.isEmpty {
                VStack(spacing: 10) {
                    Image(systemName: "equal.circle")
                        .font(.system(size: 40, weight: .light))
                        .foregroundStyle(Theme.textFaint)
                    Text("\(minDeltaMib) MiB 이상 변한 항목이 없습니다")
                        .foregroundStyle(Theme.textDim)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if let path = drillPath {
                drillSection(path)
            } else if displayMode == .treemap {
                DeltaTreemapView(entries: entries, scanTarget: scanTarget) { path in
                    enterDrill(path)
                }
            } else {
                entryList
            }
        }
        .onAppear { reload() }
        .onChange(of: appModel.scanSeconds) { reload() }
    }

    private var controlBar: some View {
        HStack(spacing: 10) {
            InstrumentLabel(text: "기준")
            Picker("", selection: $oldID) {
                ForEach(snapshots.dropFirst(), id: \.scanId) { snap in
                    Text("#\(snap.scanId) \(shortDate(snap.createdAt))")
                        .tag(Optional(snap.scanId))
                }
            }
            .labelsHidden()
            .frame(maxWidth: 190)
            .onChange(of: oldID) { computeDiff() }
            Image(systemName: "arrow.right")
                .font(.system(size: 9, weight: .bold))
                .foregroundStyle(Theme.textFaint)
            if let latest = snapshots.first {
                Text("#\(latest.scanId) \(shortDate(latest.createdAt)) (최신)")
                    .font(.dataCell)
                    .foregroundStyle(Theme.text)
            }

            InstrumentLabel(text: "최소")
            Picker("", selection: $minDeltaMib) {
                Text("10 MiB").tag(UInt64(10))
                Text("100 MiB").tag(UInt64(100))
                Text("1 GiB").tag(UInt64(1024))
            }
            .labelsHidden()
            .frame(width: 92)
            .onChange(of: minDeltaMib) { computeDiff() }

            Spacer()

            // 범례 (2계열 — 항상 표시)
            HStack(spacing: 10) {
                legendChip(color: Theme.deltaGrow, label: "증가")
                legendChip(color: Theme.deltaShrink, label: "감소")
            }

            if !entries.isEmpty {
                InstrumentLabel(text: "순변화")
                Text("\(netDelta >= 0 ? "+" : "−")\(humanBytes(UInt64(abs(netDelta))))")
                    .font(.dataCell)
                    .foregroundStyle(netDelta >= 0 ? Theme.deltaGrow : Theme.deltaShrink)
            }

            Picker("", selection: $displayMode) {
                Image(systemName: "list.bullet").tag(DisplayMode.list)
                Image(systemName: "square.grid.2x2").tag(DisplayMode.treemap)
            }
            .pickerStyle(.segmented)
            .frame(width: 76)
            Button {
                reload()
            } label: {
                Image(systemName: "arrow.clockwise")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(Theme.textDim)
            }
            .buttonStyle(.plain)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(Theme.panel)
    }

    private var entryList: some View {
        List {
            Section {
                ForEach(Array(entries.enumerated()), id: \.offset) { _, entry in
                    entryRow(entry)
                        .listRowBackground(Theme.bg)
                }
            } header: {
                InstrumentLabel(text: "변화 귀속 — 항목끼리 겹치지 않음")
            }
        }
        .listStyle(.inset)
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
    }

    private func legendChip(color: Color, label: String) -> some View {
        HStack(spacing: 4) {
            RoundedRectangle(cornerRadius: 2).fill(color).frame(width: 9, height: 9)
            Text(label).font(.system(size: 10)).foregroundStyle(Theme.textDim)
        }
    }

    private func entryRow(_ entry: DiffEntryInfo) -> some View {
        let grew = entry.delta >= 0
        // 바 길이 = 절대 delta (sqrt 스케일 — 수십 배 차이에서도 작은 항목이 보이게).
        let fraction = (Double(abs(entry.delta)) / Double(maxAbsDelta)).squareRoot()
        return HStack(spacing: 10) {
            Text("\(grew ? "+" : "−")\(humanBytes(UInt64(abs(entry.delta))))")
                .font(.dataCell)
                .foregroundStyle(grew ? Theme.deltaGrow : Theme.deltaShrink)
                .frame(width: 90, alignment: .trailing)
            VStack(alignment: .leading, spacing: 1) {
                Text(entry.path)
                    .font(.pathCell)
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                    .truncationMode(.head)
                Text("\(humanBytes(entry.beforeTotal)) → \(humanBytes(entry.afterTotal))")
                    .font(.pathCell)
                    .foregroundStyle(Theme.textFaint)
            }
            if entry.isResidual {
                TagBadge(text: "직속", color: Theme.info)
                    .help("하위 디렉토리로 설명되지 않는, 이 디렉토리 직속 파일들의 변화")
            }
            Spacer()
            Image(systemName: "chevron.right")
                .font(.system(size: 9, weight: .semibold))
                .foregroundStyle(Theme.textFaint)
        }
        .padding(.vertical, 3)
        .padding(.horizontal, 6)
        .background(alignment: .leading) {
            // 행 배경 바: 길이 = 절대량(sqrt), 색 불투명도 = 상대 변화율.
            GeometryReader { geo in
                UnevenRoundedRectangle(
                    topLeadingRadius: 0, bottomLeadingRadius: 0,
                    bottomTrailingRadius: 3, topTrailingRadius: 3
                )
                .fill(Self.deltaColor(
                    delta: entry.delta, before: entry.beforeTotal, opacityRange: 0.10...0.30))
                .frame(width: max(2, geo.size.width * fraction))
            }
        }
        .contentShape(Rectangle())
        .onTapGesture {
            enterDrill(segments(fromDiffPath: entry.path))
        }
        .contextMenu {
            Button("Finder에서 보기") {
                // diff 경로는 루트 이름부터의 상대 경로 — 스캔 루트의 부모에 붙인다.
                let base = (scanTarget as NSString).deletingLastPathComponent
                NSWorkspace.shared.activateFileViewerSelecting(
                    [URL(fileURLWithPath: base).appendingPathComponent(entry.path)])
            }
        }
    }

    private func reload() {
        isLoading = true
        loadError = nil
        let root = scanTarget
        Task {
            do {
                let snaps = try await Task.detached {
                    try listSnapshots(dbPath: AppModel.dbPath, rootPath: root)
                }.value
                self.snapshots = snaps
                if self.oldID == nil || !snaps.dropFirst().contains(where: { $0.scanId == self.oldID }) {
                    self.oldID = snaps.dropFirst().first?.scanId
                }
                self.computeDiff()
            } catch {
                self.loadError = "\(error)"
                self.isLoading = false
            }
        }
    }

    private func computeDiff() {
        guard let newest = snapshots.first, let old = oldID, snapshots.count >= 2 else {
            entries = []
            isLoading = false
            return
        }
        isLoading = true
        drillPath = nil
        let minMib = minDeltaMib
        Task {
            do {
                // 핸들에 두 트리를 상주시켜 culprits와 drilldown이 재로드 없이 동작.
                let handle = try await Task.detached {
                    try openDiff(dbPath: AppModel.dbPath, oldId: old, newId: newest.scanId)
                }.value
                self.diffHandle = handle
                self.entries = await Task.detached { handle.culprits(minDeltaMib: minMib) }.value
            } catch {
                self.loadError = "\(error)"
            }
            self.isLoading = false
        }
    }

    // MARK: - Drilldown

    private func drillSection(_ path: [String]) -> some View {
        let maxAbs = drillChildren.map { abs($0.delta) }.max() ?? 1
        let rootName = (scanTarget as NSString).lastPathComponent
        return VStack(spacing: 0) {
            // 탐색 breadcrumb
            HStack(spacing: 4) {
                Button {
                    drillPath = nil
                } label: {
                    HStack(spacing: 3) {
                        Image(systemName: "chevron.left")
                            .font(.system(size: 9, weight: .bold))
                        Text("범인 목록")
                    }
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(Theme.accent)
                }
                .buttonStyle(.plain)
                Rectangle().fill(Theme.border).frame(width: 1, height: 14)
                    .padding(.horizontal, 4)
                Button(rootName) { enterDrill([]) }
                    .buttonStyle(.plain)
                    .font(.system(size: 11.5, design: .monospaced))
                    .foregroundStyle(path.isEmpty ? Theme.text : Theme.textDim)
                ForEach(Array(path.enumerated()), id: \.offset) { depth, name in
                    Image(systemName: "chevron.compact.right")
                        .font(.system(size: 9))
                        .foregroundStyle(Theme.textFaint)
                    Button(name) { enterDrill(Array(path.prefix(depth + 1))) }
                        .buttonStyle(.plain)
                        .font(.system(
                            size: 11.5,
                            weight: depth == path.count - 1 ? .semibold : .regular,
                            design: .monospaced))
                        .foregroundStyle(depth == path.count - 1 ? Theme.text : Theme.textDim)
                }
                Spacer()
                if let handle = diffHandle {
                    let totals = handle.totals(path: path)
                    Text("\(humanBytes(totals.before)) → \(humanBytes(totals.after))")
                        .font(.dataCell)
                        .foregroundStyle(Theme.textDim)
                    Text("\(totals.delta >= 0 ? "+" : "−")\(humanBytes(UInt64(abs(totals.delta))))")
                        .font(.dataCell)
                        .foregroundStyle(totals.delta >= 0 ? Theme.deltaGrow : Theme.deltaShrink)
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .background(Theme.panel)
            Rectangle().fill(Theme.border).frame(height: 1)

            List {
                ForEach(Array(drillChildren.enumerated()), id: \.offset) { _, child in
                    drillRow(child, path: path, maxAbs: maxAbs)
                        .listRowBackground(Theme.bg)
                }
            }
            .listStyle(.inset)
            .scrollContentBackground(.hidden)
            .background(Theme.bg)
        }
    }

    private func drillRow(_ child: DiffChildInfo, path: [String], maxAbs: Int64) -> some View {
        let grew = child.delta >= 0
        let isRest = child.kind == "rest"
        let isFile = child.kind == "file"
        let fraction = child.delta == 0
            ? 0.0
            : (Double(abs(child.delta)) / Double(max(maxAbs, 1))).squareRoot()
        return HStack(spacing: 10) {
            Text(
                child.delta == 0
                    ? "±0"
                    : "\(grew ? "+" : "−")\(humanBytes(UInt64(abs(child.delta))))"
            )
            .font(.dataCell)
            .foregroundStyle(
                child.delta == 0
                    ? Theme.textFaint
                    : (grew ? Theme.deltaGrow : Theme.deltaShrink)
            )
            .frame(width: 90, alignment: .trailing)

            // 행 종류를 아이콘으로 구분: 디렉토리 / 파일 / 요약.
            Image(systemName: isRest ? "ellipsis.circle" : (isFile ? "doc" : "folder.fill"))
                .font(.system(size: 10))
                .foregroundStyle(
                    isRest
                        ? Theme.textFaint
                        : (isFile ? Theme.textDim : TreemapView.color(for: child.name)))
                .frame(width: 16)

            VStack(alignment: .leading, spacing: 1) {
                if isRest {
                    Text("그 외 작은 파일들의 변화 합")
                        .font(.system(size: 12))
                        .italic()
                        .foregroundStyle(Theme.textDim)
                } else {
                    Text(child.name)
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundStyle(Theme.text)
                        .lineLimit(1)
                }
                Text(
                    "\(humanBytes(child.before)) → \(humanBytes(child.after))"
                        + (isRest ? "  ·  개별 표시 임계값(50 MiB) 미만 파일들의 합계" : "")
                )
                .font(.pathCell)
                .foregroundStyle(Theme.textFaint)
            }
            if isFile {
                TagBadge(
                    text: child.before == 0 ? "새 파일" : (child.after == 0 ? "삭제됨" : "변경"),
                    color: child.before == 0
                        ? Theme.deltaGrow
                        : (child.after == 0 ? Theme.deltaShrink : Theme.info))
            }
            Spacer()
            if child.hasChildren {
                Image(systemName: "chevron.right")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(Theme.textFaint)
            }
        }
        .padding(.vertical, 3)
        .padding(.horizontal, 6)
        .background(alignment: .leading) {
            GeometryReader { geo in
                UnevenRoundedRectangle(
                    topLeadingRadius: 0, bottomLeadingRadius: 0,
                    bottomTrailingRadius: 3, topTrailingRadius: 3
                )
                .fill(Self.deltaColor(
                    delta: child.delta, before: child.before, opacityRange: 0.10...0.30))
                .frame(width: max(child.delta == 0 ? 0 : 2, geo.size.width * fraction))
            }
        }
        .contentShape(Rectangle())
        .onTapGesture {
            guard child.hasChildren, child.kind == "dir" else { return }
            enterDrill(path + [child.name])
        }
        .contextMenu {
            if !isRest {
                Button("Finder에서 보기") {
                    let url = URL(fileURLWithPath: scanTarget)
                        .appendingPathComponent((path + [child.name]).joined(separator: "/"))
                    NSWorkspace.shared.activateFileViewerSelecting([url])
                }
            }
        }
    }

    private func shortDate(_ iso: String) -> String {
        // DB의 UTC ISO8601을 로컬 시각으로 표시.
        guard let date = ISO8601DateFormatter().date(from: iso) else { return iso }
        let formatter = DateFormatter()
        formatter.dateFormat = "MM-dd HH:mm"
        return formatter.string(from: date)
    }
}

/// 변화만 보이는 트리맵 — 면적 = |delta|, 색 = 방향(코퍼/틸) × 상대 변화율(불투명도).
/// diff 항목은 서로 겹치지 않으므로(잔차 귀속) 면적 합이 총 변화량과 일치한다.
struct DeltaTreemapView: View {
    let entries: [DiffEntryInfo]
    let scanTarget: String
    /// 타일 클릭 → 해당 경로(루트 아래 세그먼트)로 drilldown.
    let onSelect: ([String]) -> Void

    @State private var hoveredIndex: Int?

    private let maxTiles = 50

    var body: some View {
        VStack(spacing: 0) {
            GeometryReader { geo in
                let tiles = computeTiles(in: CGRect(origin: .zero, size: geo.size))
                Canvas { ctx, _ in
                    for (index, tile) in tiles.enumerated() {
                        // 2px surface gap (mark spec).
                        let r = tile.rect.insetBy(dx: 1, dy: 1)
                        guard r.width > 1, r.height > 1 else { continue }
                        ctx.fill(
                            Path(roundedRect: r, cornerRadius: 2),
                            with: .color(
                                ChangesView.deltaColor(
                                    delta: tile.entry.delta, before: tile.entry.beforeTotal,
                                    opacityRange: 0.38...0.95))
                        )
                        if index == hoveredIndex {
                            ctx.stroke(
                                Path(roundedRect: r.insetBy(dx: 1, dy: 1), cornerRadius: 2),
                                with: .color(Theme.text),
                                lineWidth: 2
                            )
                        }
                        if r.width > 70, r.height > 32 {
                            let name = (tile.entry.path as NSString).lastPathComponent
                            let grew = tile.entry.delta >= 0
                            let label = Text(name)
                                .font(.system(size: 10, weight: .semibold))
                                .foregroundStyle(Theme.text)
                            ctx.draw(
                                label,
                                in: CGRect(x: r.minX + 6, y: r.minY + 5, width: r.width - 12, height: 14))
                            let delta = Text(
                                "\(grew ? "+" : "−")\(humanBytes(UInt64(abs(tile.entry.delta))))"
                            )
                            .font(.system(size: 9.5, design: .monospaced))
                            .foregroundStyle(Theme.text.opacity(0.8))
                            ctx.draw(
                                delta,
                                in: CGRect(x: r.minX + 6, y: r.minY + 19, width: r.width - 12, height: 13))
                        }
                    }
                }
                .onContinuousHover { phase in
                    switch phase {
                    case .active(let location):
                        hoveredIndex = tiles.firstIndex(where: { $0.rect.contains(location) })
                    case .ended:
                        hoveredIndex = nil
                    }
                }
                .gesture(
                    SpatialTapGesture().onEnded { value in
                        guard let tile = tiles.first(where: { $0.rect.contains(value.location) })
                        else { return }
                        onSelect(
                            tile.entry.path.split(separator: "/").dropFirst().map(String.init))
                    }
                )
            }
            .background(Theme.bg)
            Rectangle().fill(Theme.border).frame(height: 1)
            hoverStrip
        }
    }

    private struct DeltaTile {
        let rect: CGRect
        let entry: DiffEntryInfo
    }

    private func computeTiles(in rect: CGRect) -> [DeltaTile] {
        // entries는 이미 |delta| 내림차순 — squarified 입력 조건 충족.
        let visible = Array(entries.prefix(maxTiles))
        guard !visible.isEmpty else { return [] }
        let rects = Squarify.layout(
            values: visible.map { CGFloat(abs($0.delta)) }, in: rect)
        return zip(visible, rects).map { DeltaTile(rect: $1, entry: $0) }
    }

    /// 호버된 타일의 전체 경로 + 전/후 컨텍스트 (툴팁 대체 상시 스트립).
    private var hoverStrip: some View {
        HStack(spacing: 8) {
            if let index = hoveredIndex, index < min(entries.count, maxTiles) {
                let entry = entries[index]
                Text(entry.path)
                    .font(.pathCell)
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                    .truncationMode(.head)
                Spacer()
                Text("\(humanBytes(entry.beforeTotal)) → \(humanBytes(entry.afterTotal))")
                    .font(.dataCell)
                    .foregroundStyle(Theme.textDim)
                if entry.isResidual {
                    TagBadge(text: "직속", color: Theme.info)
                }
                Button {
                    let base = (scanTarget as NSString).deletingLastPathComponent
                    NSWorkspace.shared.activateFileViewerSelecting(
                        [URL(fileURLWithPath: base).appendingPathComponent(entry.path)])
                } label: {
                    Image(systemName: "folder")
                        .font(.system(size: 10))
                        .foregroundStyle(Theme.textDim)
                }
                .buttonStyle(.plain)
                .help("Finder에서 보기")
            } else {
                Text(
                    entries.count > maxTiles
                        ? "상위 \(maxTiles)개 표시 (전체 \(entries.count)개 중) — 호버=상세, 클릭=안으로 들어가기"
                        : "타일에 마우스를 올리면 상세, 클릭하면 안으로 들어갑니다"
                )
                .font(.system(size: 10.5))
                .foregroundStyle(Theme.textFaint)
                Spacer()
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(Theme.panel)
    }
}
