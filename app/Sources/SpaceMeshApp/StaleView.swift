import AppKit
import QuickLook
import SpaceMeshCore
import SwiftUI

/// 방치 대용량 — 크고(≥10MiB) 오래(180일+) 안 건드린 파일 랭킹 (크기×방치일 순).
/// 트리맵 사이드바에 숨어 있던 섹션을 회수 그룹의 1급 뷰로 승격한 화면.
/// 삭제는 다른 회수 뷰와 동일하게 CartBar → 휴지통(복원 가능) 경유.
struct StaleView: View {
    @EnvironmentObject var app: AppModel
    @ObservedObject var cleanup: CleanupModel

    @State private var selected: Set<String> = []
    @State private var confirmTrash = false
    @State private var previewURL: URL?

    private var selectedItems: [(path: String, size: UInt64)] {
        app.staleFiles
            .filter { selected.contains($0.path) }
            .map { ($0.path, $0.allocatedSize) }
    }
    private var selectedSize: UInt64 { selectedItems.reduce(0) { $0 + $1.size } }
    private var totalSize: UInt64 { app.staleFiles.reduce(0) { $0 + $1.allocatedSize } }

    var body: some View {
        VStack(spacing: 0) {
            if app.handle == nil {
                VStack(spacing: 12) {
                    Image(systemName: "clock.badge.exclamationmark")
                        .font(.system(size: 40))
                        .foregroundStyle(.secondary)
                    Text("방치 대용량 분석은 스캔 결과를 사용합니다")
                        .foregroundStyle(.secondary)
                    Button("스캔 실행") {
                        app.startScan(path: app.scannedRoot.isEmpty
                            ? NSHomeDirectory() : app.scannedRoot)
                    }
                    .buttonStyle(.borderedProminent)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if app.isScanning {
                ScanningView(startedAt: app.scanStartedAt, label: "스캔 중")
            } else if app.staleFiles.isEmpty {
                VStack(spacing: 10) {
                    Image(systemName: "checkmark.seal")
                        .font(.system(size: 40))
                        .foregroundStyle(.secondary)
                    Text("\(AppModel.staleAgeDays)일 이상 방치된 대용량 파일이 없습니다")
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                summaryBar
                Divider()
                fileList
            }
            Divider()
            CartBar(
                selectedCount: selectedItems.count,
                selectedSize: selectedSize,
                message: cleanup.message,
                undoAvailable: !cleanup.lastBatch.isEmpty,
                onTrash: { confirmTrash = true },
                onUndo: { cleanup.undoLastBatch() },
                onRefresh: { app.refreshStale() }
            )
        }
        .quickLookPreview($previewURL)
        .confirmationDialog(
            "\(selectedItems.count)개 방치 파일 (\(humanBytes(selectedSize)))을 휴지통으로 이동할까요?",
            isPresented: $confirmTrash, titleVisibility: .visible
        ) {
            Button("휴지통으로 이동", role: .destructive) {
                let paths = Set(selectedItems.map(\.path))
                cleanup.trash(paths: selectedItems)
                app.removeStale(paths: paths)
                selected = []
            }
        } message: {
            Text("휴지통에서 언제든 복원할 수 있고, 이동 직후에는 되돌리기 버튼도 사용할 수 있습니다.")
        }
    }

    private var summaryBar: some View {
        HStack(spacing: 10) {
            InstrumentLabel(text: "마지막 수정 \(AppModel.staleAgeDays)일+ 경과 · 크기×경과일 순")
            Spacer()
            InstrumentLabel(text: "\(app.staleFiles.count)개")
            Text(humanBytes(totalSize))
                .font(.dataCell)
                .foregroundStyle(Theme.accent)
        }
        .help("기준은 mtime(마지막 내용 수정)입니다. 읽기만 한 파일은 여기 나타날 수 있으니 검토 후보로만 보세요 — macOS에서 '마지막 열람'(atime)은 인덱싱·백업이 건드려 신뢰할 수 없습니다.")
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(Theme.panel)
    }

    private var fileList: some View {
        List {
            ForEach(app.staleFiles, id: \.path) { file in
                staleRow(file)
                    .listRowBackground(Theme.bg)
            }
        }
        .listStyle(.inset)
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
    }

    private func staleRow(_ file: BigFile) -> some View {
        HStack(spacing: 10) {
            Toggle(
                "",
                isOn: Binding(
                    get: { selected.contains(file.path) },
                    set: { on in
                        if on { selected.insert(file.path) } else { selected.remove(file.path) }
                    }
                )
            )
            .labelsHidden()
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
            if let age = ageDaysLabel(file.modifiedEpoch) {
                TagBadge(text: age, color: Theme.warn)
            }
            Text(humanBytes(file.allocatedSize))
                .font(.dataCell)
                .foregroundStyle(Theme.text)
        }
        .contentShape(Rectangle())
        .onTapGesture(count: 2) {
            previewURL = URL(fileURLWithPath: file.path)
        }
        .contextMenu {
            Button("Quick Look") { previewURL = URL(fileURLWithPath: file.path) }
            Button("Finder에서 보기") {
                NSWorkspace.shared.activateFileViewerSelecting(
                    [URL(fileURLWithPath: file.path)])
            }
        }
    }
}
