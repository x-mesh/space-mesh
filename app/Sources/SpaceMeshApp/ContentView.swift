import AppKit
import QuickLook
import SpaceMeshCore
import SwiftUI

enum ViewMode: String, CaseIterable {
    case treemap = "트리맵"
    case changes = "변화"
    case categories = "빌드 산출물"
    case git = "Git"
    case cleanup = "정리"
    case duplicates = "중복"
}

struct ContentView: View {
    @EnvironmentObject var model: AppModel
    @StateObject private var cleanup = CleanupModel()
    @State private var scanTarget = NSHomeDirectory()
    @State private var previewURL: URL?
    @State private var mode: ViewMode = .treemap

    var body: some View {
        VStack(spacing: 0) {
            toolbar
            Rectangle().fill(Theme.border).frame(height: 1)
            switch mode {
            case .treemap:
                treemapSection
            case .changes:
                ChangesView(scanTarget: scanTarget)
            case .categories:
                CategoriesView(model: cleanup, scanTarget: scanTarget)
            case .git:
                GitView(scanTarget: scanTarget)
            case .cleanup:
                CleanupView(model: cleanup)
            case .duplicates:
                DuplicatesView(model: cleanup, defaultRoot: scanTarget)
            }
        }
        .background(Theme.bg)
        .preferredColorScheme(.dark)
        .tint(Theme.accent)
        .quickLookPreview($previewURL)
    }

    @ViewBuilder
    private var treemapSection: some View {
        if model.isScanning {
            ScanningView(startedAt: model.scanStartedAt, label: "스캔 중")
        } else if model.handle == nil {
            emptyState
        } else {
            breadcrumbBar
            Rectangle().fill(Theme.border).frame(height: 1)
            HSplitView {
                TreemapView()
                    .frame(minWidth: 480)
                sidebar
                    .frame(minWidth: 260, maxWidth: 380)
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
                mode = .treemap
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

            ModeTabs(mode: $mode)

            Spacer()

            if let stats = model.stats, let secs = model.scanSeconds, !model.isScanning {
                HStack(spacing: 12) {
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

    private func readoutItem(value: String, label: String) -> some View {
        VStack(alignment: .trailing, spacing: 0) {
            Text(value)
                .font(.dataCell)
                .foregroundStyle(Theme.textDim)
            Text(label)
                .font(.system(size: 7.5, weight: .semibold))
                .tracking(1.0)
                .foregroundStyle(Theme.textFaint)
        }
    }

    // MARK: - 빈 상태

    private var emptyState: some View {
        VStack(spacing: 16) {
            if let error = model.errorMessage {
                Text(error).font(.callout).foregroundStyle(Theme.warn)
            }
            Image(systemName: "internaldrive")
                .font(.system(size: 42, weight: .light))
                .foregroundStyle(Theme.textFaint)
            VStack(spacing: 6) {
                Text("디스크가 어디에 쓰이고 있는지 확인하세요")
                    .font(.title3.weight(.semibold))
                    .foregroundStyle(Theme.text)
                Text("경로를 정하고 SCAN — 트리맵으로 탐색하고, 빌드 산출물·정리·중복 탭에서 회수합니다")
                    .font(.callout)
                    .foregroundStyle(Theme.textDim)
            }
        }
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
            if !model.staleFiles.isEmpty {
                Section {
                    ForEach(model.staleFiles, id: \.path) { file in
                        HStack(spacing: 8) {
                            Image(systemName: "clock.badge.exclamationmark")
                                .font(.system(size: 10))
                                .foregroundStyle(Theme.warn)
                            VStack(alignment: .leading, spacing: 1) {
                                Text((file.path as NSString).lastPathComponent)
                                    .font(.system(size: 12))
                                    .foregroundStyle(Theme.text)
                                    .lineLimit(1)
                                Text((file.path as NSString).deletingLastPathComponent)
                                    .font(.pathCell)
                                    .foregroundStyle(Theme.textFaint)
                                    .lineLimit(1)
                            }
                            Spacer()
                            VStack(alignment: .trailing, spacing: 1) {
                                Text(humanBytes(file.allocatedSize))
                                    .font(.dataCell)
                                    .foregroundStyle(Theme.textDim)
                                if let age = ageDaysLabel(file.modifiedEpoch) {
                                    Text(age)
                                        .font(.pathCell)
                                        .foregroundStyle(Theme.warn)
                                }
                            }
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
                    InstrumentLabel(
                        text: "방치 대용량 · \(AppModel.staleAgeDays)일+ · 크기×방치일 순")
                }
            }
        }
        .listStyle(.inset)
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
    }
}

/// 계기판 스타일 모드 전환 탭 — 대문자 라벨 + 앰버 언더라인.
struct ModeTabs: View {
    @Binding var mode: ViewMode

    var body: some View {
        HStack(spacing: 2) {
            ForEach(ViewMode.allCases, id: \.self) { m in
                Button {
                    mode = m
                } label: {
                    VStack(spacing: 4) {
                        Text(m.rawValue)
                            .font(.system(size: 11, weight: mode == m ? .bold : .medium))
                            .foregroundStyle(mode == m ? Theme.text : Theme.textDim)
                        Rectangle()
                            .fill(mode == m ? Theme.accent : .clear)
                            .frame(height: 2)
                    }
                    .padding(.horizontal, 8)
                    .padding(.top, 4)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
    }
}
