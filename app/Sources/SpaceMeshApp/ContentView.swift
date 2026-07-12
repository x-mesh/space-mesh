import AppKit
import QuickLook
import SpaceMeshCore
import SwiftUI

/// 사이드바 항목 — 유저 job(진단 → 회수 → 안전) 기준 3그룹.
enum SidebarItem: String, CaseIterable, Identifiable {
    // 진단: 어디가 크고, 뭐가 늘었나
    case treemap, changes
    // 회수: 뭘 지울까
    case categories, apps, duplicates, stale, cleanup
    // 안전: 지워도 되나
    case git

    var id: String { rawValue }

    var title: String {
        switch self {
        case .treemap: return "트리맵"
        case .changes: return "변화"
        case .categories: return "빌드 산출물"
        case .apps: return "앱"
        case .duplicates: return "중복"
        case .stale: return "미수정 180일+"
        case .cleanup: return "캐시·도구"
        case .git: return "Git"
        }
    }

    var icon: String {
        switch self {
        case .treemap: return "square.grid.3x3.topleft.filled"
        case .changes: return "clock.arrow.circlepath"
        case .categories: return "hammer"
        case .apps: return "square.grid.2x2"
        case .duplicates: return "doc.on.doc"
        case .stale: return "clock.badge.exclamationmark"
        case .cleanup: return "sparkles"
        case .git: return "arrow.triangle.branch"
        }
    }
}

/// 사이드바 그룹 정의 (표시 순서 고정).
private let sidebarGroups: [(title: String, items: [SidebarItem])] = [
    ("진단", [.treemap, .changes]),
    ("회수", [.categories, .apps, .duplicates, .stale, .cleanup]),
    ("안전", [.git]),
]

struct ContentView: View {
    @EnvironmentObject var model: AppModel
    @StateObject private var cleanup = CleanupModel()
    @State private var scanTarget = NSHomeDirectory()
    @State private var previewURL: URL?
    @State private var selection: SidebarItem = .treemap

    var body: some View {
        GeometryReader { geometry in
            let sidebarWidth: CGFloat = geometry.size.width < 1080 ? 224 : 240
            let detailWidth = max(0, geometry.size.width - sidebarWidth - 1)

            VStack(spacing: 0) {
                toolbar
                Rectangle().fill(Theme.border).frame(height: 1)
                HStack(spacing: 0) {
                    sidebarNav
                        .frame(width: sidebarWidth)
                        .fixedSize(horizontal: true, vertical: false)
                        .layoutPriority(10)
                    Rectangle().fill(Theme.border).frame(width: 1)
                    detail
                        .frame(width: detailWidth)
                        .frame(maxHeight: .infinity)
                        .clipped()
                        .layoutPriority(0)
                }
            }
        }
        .background(Theme.bg)
        .preferredColorScheme(.dark)
        .tint(Theme.accent)
        .quickLookPreview($previewURL)
    }

    @ViewBuilder
    private var detail: some View {
        // 달 배경은 .background로 둔다. ZStack 형제로 놓으면 scaledToFill이
        // 콘텐츠보다 큰 레이아웃 크기를 보고해 detail 폭을 넘겨버리고,
        // 그 결과 좁은 창에서 트리맵·인스펙터가 양쪽으로 잘린다.
        Group {
            switch selection {
            case .treemap:
                treemapSection
            case .changes:
                ChangesView(scanTarget: scanTarget)
            case .categories:
                CategoriesView(model: cleanup, scanTarget: scanTarget)
            case .apps:
                AppsView(model: cleanup)
            case .git:
                GitView(scanTarget: scanTarget)
            case .cleanup:
                CleanupView(model: cleanup)
            case .duplicates:
                DuplicatesView(model: cleanup, defaultRoot: scanTarget)
            case .stale:
                StaleView(cleanup: cleanup)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background {
            // scaledToFill 달 이미지(16:9)는 좁은 창에서 detail보다 가로로 넘친다.
            // .clipped()는 그리기만 자르고 hit-testing은 안 자르며, detail은
            // HStack에서 사이드바보다 뒤(위)에 그려진다 — 그래서 눈엔 안 보이는
            // 좌측 초과분이 왼쪽 '회수' 메뉴들의 클릭을 가로챈다. 장식 배경이므로
            // 아예 hit-testing에서 제외한다.
            MoonBackdrop()
                .opacity(0.52)
                .overlay(Color.black.opacity(0.34))
                .allowsHitTesting(false)
        }
        .clipped()
    }

    // MARK: - 사이드바 (job 단계: 진단 → 회수 → 안전)

    private var sidebarNav: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(sidebarGroups, id: \.title) { group in
                HStack(spacing: 6) {
                    InstrumentLabel(text: group.title)
                    Spacer()
                    // 회수 그룹 헤더 = 상시 지표 (안전 회수 가능 합계).
                    if group.title == "회수", let reclaim = model.reclaimSummary,
                        reclaim.safeTotal > 0
                    {
                        Text(humanBytes(reclaim.safeTotal))
                            .font(.dataCell)
                            .foregroundStyle(Theme.accent)
                            .help("안전 회수 가능 합계 — 빌드 산출물 \(reclaim.hitCount)곳")
                    }
                }
                .padding(.horizontal, 14)
                .padding(.top, 18)
                .padding(.bottom, 6)
                ForEach(group.items) { item in
                    sidebarRow(item)
                }
            }
            Spacer()
            // 스캔 중에는 어느 뷰에 있든 진행 상황을 왼쪽 아래에 상시 표시.
            if model.isScanning {
                scanFooter
            }
        }
        .background(Theme.panel)
    }

    /// 사이드바 하단 스캔 진행 인디케이터 — 클릭하면 레이더 화면으로 복귀.
    private var scanFooter: some View {
        TimelineView(.periodic(from: .now, by: 0.5)) { timeline in
            let elapsed = max(0, timeline.date.timeIntervalSince(model.scanStartedAt))
            Button {
                selection = .treemap
            } label: {
                VStack(alignment: .leading, spacing: 4) {
                    HStack(spacing: 6) {
                        ProgressView()
                            .controlSize(.small)
                        InstrumentLabel(text: "스캔 중")
                        Spacer()
                    }
                    Text("\(scanProgress().formatted()) files")
                        .font(.dataCell)
                        .foregroundStyle(Theme.text)
                        .contentTransition(.numericText())
                    Text(String(format: "%.0fs 경과", elapsed))
                        .font(.pathCell)
                        .foregroundStyle(Theme.textFaint)
                }
                .padding(10)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Theme.raised)
                .overlay(Rectangle().stroke(Theme.border, lineWidth: 1))
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .padding(8)
            .help("스캔 진행 중 — 클릭하면 진행 화면으로 이동")
        }
    }

    private func sidebarRow(_ item: SidebarItem) -> some View {
        let selected = selection == item
        return Button {
            selection = item
        } label: {
            HStack(spacing: 8) {
                Image(systemName: item.icon)
                    .font(.system(size: 12))
                    .frame(width: 18)
                    .foregroundStyle(selected ? Theme.accent : Theme.textDim)
                Text(item.title)
                    .font(.system(size: 12.5, weight: selected ? .semibold : .regular))
                    .foregroundStyle(selected ? Theme.text : Theme.textDim)
                Spacer()
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .background(
                selected ? Theme.accentSoft : .clear,
            )
            .overlay(Rectangle().stroke(selected ? Theme.border : .clear, lineWidth: 1))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .padding(.horizontal, 8)
    }

    @ViewBuilder
    private var treemapSection: some View {
        if model.isScanning {
            ScanningView(startedAt: model.scanStartedAt, label: "스캔 중")
        } else if model.handle == nil {
            emptyState
        } else {
            // VStack 필수 — detail이 바깥 HStack 안에 있어, 감싸지 않으면
            // breadcrumb·divider·treemap이 가로로 나열된다.
            GeometryReader { geometry in
                VStack(spacing: 0) {
                    breadcrumbBar
                    Rectangle().fill(Theme.border).frame(height: 1)
                    if geometry.size.width < 820 {
                        TreemapView()
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                    } else {
                        HSplitView {
                            TreemapView()
                                .frame(minWidth: 480)
                            sidebar
                                .frame(minWidth: 240, idealWidth: 300, maxWidth: 360)
                        }
                    }
                }
            }
        }
    }

    // MARK: - 툴바

    private var toolbar: some View {
        HStack(spacing: 10) {
            HStack(spacing: 6) {
                Image(systemName: "folder")
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.textFaint)
                TextField("스캔할 경로", text: $scanTarget)
                    .textFieldStyle(.plain)
                    .font(.pathCell)
                    .foregroundStyle(Theme.text)
                Button {
                    let panel = NSOpenPanel()
                    panel.canChooseDirectories = true
                    panel.canChooseFiles = false
                    if panel.runModal() == .OK, let url = panel.url {
                        scanTarget = url.path
                    }
                } label: {
                    Image(systemName: "ellipsis")
                        .font(.system(size: 10, weight: .bold))
                        .foregroundStyle(Theme.textDim)
                }
                .buttonStyle(.plain)
                .disabled(model.isScanning)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .background(Theme.raised, in: RoundedRectangle(cornerRadius: 6))
            .frame(maxWidth: 340)

            Button {
                selection = .treemap
                model.startScan(path: scanTarget)
            } label: {
                Text("SCAN")
                    .font(.system(size: 11, weight: .bold))
                    .tracking(1.2)
                    .foregroundStyle(model.isScanning ? Theme.textFaint : Theme.bg)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 6)
                    .background(
                        model.isScanning ? Theme.raised : Theme.accent,
                        in: RoundedRectangle(cornerRadius: 6)
                    )
            }
            .buttonStyle(.plain)
            .keyboardShortcut(.return, modifiers: .command)
            .disabled(model.isScanning)

            Spacer()

            if let stats = model.stats, let secs = model.scanSeconds, !model.isScanning {
                HStack(spacing: 14) {
                    // 헤드라인은 "디스크에 얼마나 남았나" — 사용자가 실제로 답을 원하는 질문이다.
                    // 예전에는 스캔 루트의 크기를 "USED"라고 불렀다. 프로젝트 폴더 하나를 스캔하면
                    // "45.3 GiB USED"라고 떴는데, 디스크는 실제로 765 GB를 쓰고 있었다.
                    if model.volumeTotal > 0 {
                        readoutItem(
                            value: humanBytes(model.volumeFree), label: "FREE",
                            emphasized: true
                        )
                        .help(
                            "디스크 \(humanBytes(model.volumeUsed)) 사용 중 / 전체 \(humanBytes(model.volumeTotal))"
                        )
                        Rectangle().fill(Theme.border).frame(width: 1, height: 22)
                    }
                    // 스캔 범위 — 트리맵이 보여주는 게 디스크 전체가 아님을 분명히 한다.
                    readoutItem(value: humanBytes(model.rootAllocated), label: "SCANNED")
                        .help(scanCoverageHelp())
                    Rectangle().fill(Theme.border).frame(width: 1, height: 22)
                    readoutItem(value: stats.totalFiles.formatted(), label: "FILES")
                    readoutItem(value: stats.totalDirs.formatted(), label: "DIRS")
                    readoutItem(value: String(format: "%.1fs", secs), label: "SCAN")
                    if stats.errors > 0 {
                        readoutItem(value: stats.errors.formatted(), label: "SKIP")
                    }
                }
            }
            Button {
                if #available(macOS 14, *) {
                    NSApp.sendAction(Selector(("showSettingsWindow:")), to: nil, from: nil)
                }
            } label: {
                Image(systemName: "gearshape")
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.textDim)
            }
            .buttonStyle(.plain)
            .help("백그라운드 감시 설정")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(Theme.panel)
    }

    /// 스캔이 디스크의 얼마를 덮는지. 나머지는 스캔 범위 밖이라 트리맵에 없다.
    private func scanCoverageHelp() -> String {
        guard let coverage = model.scanCoverage else {
            return "스캔한 범위의 크기입니다 (디스크 전체 사용량이 아닙니다)"
        }
        let outside = model.volumeUsed > model.rootAllocated
            ? model.volumeUsed - model.rootAllocated : 0
        return """
            스캔 범위: 디스크 사용량의 \(Int(coverage * 100))%
            나머지 \(humanBytes(outside))는 스캔 밖이라 트리맵에 없습니다
            """
    }

    private func readoutItem(value: String, label: String, emphasized: Bool = false)
        -> some View
    {
        VStack(alignment: .trailing, spacing: 0) {
            Text(value)
                .font(
                    emphasized
                        ? .system(size: 14, weight: .semibold, design: .monospaced) : .dataCell
                )
                .foregroundStyle(emphasized ? Theme.text : Theme.textDim)
            Text(label)
                .font(.system(size: 7.5, weight: .semibold))
                .tracking(1.0)
                .foregroundStyle(emphasized ? Theme.textDim : Theme.textFaint)
        }
    }

    // MARK: - 빈 상태

    private var emptyState: some View {
        VStack(alignment: .center, spacing: 16) {
            if let error = model.errorMessage {
                Text(error)
                    .font(.callout)
                    .foregroundStyle(Theme.warn)
                    .multilineTextAlignment(.center)
            }
            Image(systemName: "internaldrive")
                .font(.system(size: 42, weight: .light))
                .foregroundStyle(Theme.accent)
                .frame(width: 56, height: 56, alignment: .center)
            VStack(spacing: 6) {
                Text("디스크의 궤도를 그립니다")
                    .font(.system(size: 22, weight: .medium, design: .rounded))
                    .tracking(1.8)
                    .foregroundStyle(Theme.text)
                    .multilineTextAlignment(.center)
                Text("경로를 정하고 SCAN — 사용량을 지도처럼 탐색하세요")
                    .font(.callout)
                    .foregroundStyle(Theme.textDim)
                    .multilineTextAlignment(.center)
            }
            InstrumentLabel(text: "SPACE-MESH // ORBIT 01")
        }
        .frame(maxWidth: 560)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    // MARK: - Breadcrumb

    private var breadcrumbBar: some View {
        HStack(spacing: 2) {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 2) {
                    crumbButton(displayRootName, depth: 0, isLast: model.breadcrumb.isEmpty)
                    ForEach(Array(model.breadcrumb.enumerated()), id: \.offset) { depth, name in
                        Image(systemName: "chevron.compact.right")
                            .font(.system(size: 9))
                            .foregroundStyle(Theme.textFaint)
                        crumbButton(
                            name, depth: depth + 1,
                            isLast: depth == model.breadcrumb.count - 1)
                    }
                }
            }
            Spacer()
            if let node = model.currentNode {
                InstrumentLabel(text: model.breadcrumb.isEmpty ? "전체" : "이 폴더")
                Text(humanBytes(node.allocatedSize))
                    .font(.dataCell)
                    .foregroundStyle(Theme.accent)
                Text("·")
                    .foregroundStyle(Theme.textFaint)
                Text("\(node.fileCount.formatted()) files")
                    .font(.dataCell)
                    .foregroundStyle(Theme.textDim)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(Theme.panel)
    }

    private func crumbButton(_ name: String, depth: Int, isLast: Bool) -> some View {
        Button {
            model.navigate(toDepth: depth)
        } label: {
            Text(name)
                .font(.system(size: 11.5, weight: isLast ? .semibold : .regular, design: .monospaced))
                .foregroundStyle(isLast ? Theme.text : Theme.textDim)
                .padding(.horizontal, 4)
                .padding(.vertical, 2)
        }
        .buttonStyle(.plain)
    }

    private var displayRootName: String {
        (scanTarget as NSString).lastPathComponent
    }

    // MARK: - 사이드바

    private var sidebar: some View {
        List {
            if !model.children.isEmpty {
                Section {
                    ForEach(model.children, id: \.index) { child in
                        HStack(spacing: 8) {
                            RoundedRectangle(cornerRadius: 2)
                                .fill(TreemapView.color(for: child.name))
                                .frame(width: 8, height: 8)
                            Text(child.name)
                                .font(.system(size: 12))
                                .foregroundStyle(Theme.text)
                                .lineLimit(1)
                            Spacer()
                            Text(humanBytes(child.allocatedSize))
                                .font(.dataCell)
                                .foregroundStyle(Theme.textDim)
                        }
                        .contentShape(Rectangle())
                        .onTapGesture { model.drill(into: child) }
                        .contextMenu {
                            Button("Finder에서 보기") {
                                if let path = model.fullPath(of: child) {
                                    NSWorkspace.shared.activateFileViewerSelecting(
                                        [URL(fileURLWithPath: path)])
                                }
                            }
                        }
                        .listRowBackground(Theme.bg)
                    }
                } header: {
                    InstrumentLabel(text: "하위 디렉토리")
                }
            }
            if !model.bigFiles.isEmpty {
                Section {
                    ForEach(model.bigFiles, id: \.path) { file in
                        HStack(spacing: 8) {
                            Image(systemName: "doc")
                                .font(.system(size: 10))
                                .foregroundStyle(Theme.textFaint)
                            Text((file.path as NSString).lastPathComponent)
                                .font(.system(size: 12))
                                .foregroundStyle(Theme.text)
                                .lineLimit(1)
                            Spacer()
                            Text(humanBytes(file.allocatedSize))
                                .font(.dataCell)
                                .foregroundStyle(Theme.textDim)
                        }
                        .contentShape(Rectangle())
                        .onTapGesture {
                            previewURL = URL(fileURLWithPath: file.path)
                        }
                        .contextMenu {
                            Button("Finder에서 보기") {
                                NSWorkspace.shared.activateFileViewerSelecting(
                                    [URL(fileURLWithPath: file.path)])
                            }
                        }
                        .listRowBackground(Theme.bg)
                    }
                } header: {
                    InstrumentLabel(text: "대용량 파일 · 직속")
                }
            }
        }
        .listStyle(.inset)
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
    }
}
