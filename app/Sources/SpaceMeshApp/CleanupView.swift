import AppKit
import SpaceMeshCore
import SwiftUI

/// 룰 기반 불필요 파일 탐지 + Cleanup Cart.
struct CleanupView: View {
    @ObservedObject var model: CleanupModel
    @State private var confirmTrash = false

    private var selected: [CleanupCandidate] {
        model.candidates.filter { model.selectedCleanupPaths.contains($0.path) }
    }
    /// 항목들은 서로 겹치지 않는다 (core에서 안쪽 항목을 빼둔다) — 그래서 그냥 더해도
    /// 실제로 비워지는 용량과 맞는다. 예전에는 ~/Library/Caches와 그 안의 Homebrew 캐시를
    /// 각각 세서 회수량을 22 GB 넘게 부풀려 말했다.
    private var selectedSize: UInt64 {
        selected.reduce(0) { $0 + $1.allocatedSize }
    }

    /// 실제 삭제 대상. 한 항목이 여러 경로로 쪼개질 수 있다 ("기타 앱 캐시"의 나머지 항목들).
    /// 크기는 항목당 한 번만 실어 합계가 두 배로 잡히지 않게 한다.
    private func trashTargets() -> [(path: String, size: UInt64)] {
        selected.flatMap { candidate -> [(path: String, size: UInt64)] in
            candidate.deletePaths.enumerated().map { index, path in
                (path: path, size: index == 0 ? candidate.allocatedSize : 0)
            }
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            if model.isDetecting {
                ScanningView(
                    startedAt: model.detectStartedAt, label: "정리 후보 측정 중", unit: "files")
            } else if model.candidates.isEmpty {
                VStack(spacing: 10) {
                    Image(systemName: "sparkles")
                        .font(.system(size: 40))
                        .foregroundStyle(.secondary)
                    Text("탐지된 정리 후보가 없습니다").foregroundStyle(.secondary)
                    Button("다시 검사") { model.detect() }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                candidateList
            }
            Divider()
            CartBar(
                selectedCount: selected.count,
                selectedSize: selectedSize,
                message: model.message,
                undoAvailable: !model.lastBatch.isEmpty,
                onTrash: { confirmTrash = true },
                onUndo: { model.undoLastBatch() },
                onRefresh: { model.detect() }
            )
        }
        .onAppear {
            if model.candidates.isEmpty && !model.isDetecting {
                model.detect()
            }
            if model.advices.isEmpty {
                model.loadAdvice()
            }
        }
        .confirmationDialog(
            "\(selected.count)개 항목 (\(humanBytes(selectedSize)))을 휴지통으로 이동할까요?",
            isPresented: $confirmTrash, titleVisibility: .visible
        ) {
            Button("휴지통으로 이동", role: .destructive) {
                // path가 아니라 deletePaths를 지운다. "기타 앱 캐시"의 path는
                // ~/Library/Caches이고, 그걸 통째로 지우면 따로 고를 수 있어야 할
                // Homebrew·pip·Yarn 캐시까지 함께 날아간다.
                model.trash(paths: trashTargets())
                model.detect()
            }
        } message: {
            Text("휴지통에서 언제든 복원할 수 있고, 이동 직후에는 되돌리기 버튼도 사용할 수 있습니다.")
        }
    }

    private var candidateList: some View {
        let grouped = Dictionary(grouping: model.candidates, by: \.category)
        return List {
            if !model.advices.isEmpty {
                Section {
                    ForEach(model.advices, id: \.command) { advice in
                        adviceRow(advice)
                            .listRowBackground(Theme.bg)
                    }
                } header: {
                    InstrumentLabel(text: "공식 정리 커맨드 — 직접 실행 (파일 삭제보다 안전)")
                }
            } else if model.isAdvising {
                Section {
                    HStack(spacing: 8) {
                        ProgressView().controlSize(.small)
                        Text("설치된 도구의 dry-run 조회 중…")
                            .font(.system(size: 11))
                            .foregroundStyle(Theme.textDim)
                    }
                    .listRowBackground(Theme.bg)
                }
            }
            ForEach(grouped.keys.sorted(), id: \.self) { category in
                Section {
                    ForEach(grouped[category] ?? [], id: \.path) { candidate in
                        candidateRow(candidate)
                            .listRowBackground(Theme.bg)
                    }
                } header: {
                    InstrumentLabel(text: categoryLabel(category))
                }
            }
        }
        .listStyle(.inset)
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
    }

    private func candidateRow(_ candidate: CleanupCandidate) -> some View {
        let selectable = candidate.category != "trash"
        return HStack(alignment: .center, spacing: 10) {
            Toggle(
                "",
                isOn: Binding(
                    get: { model.selectedCleanupPaths.contains(candidate.path) },
                    set: { on in model.toggleCleanupSelection(candidate.path, on: on) }
                )
            )
            .labelsHidden()
            .disabled(!selectable)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(candidate.title)
                        .font(.system(size: 12.5, weight: .semibold))
                        .foregroundStyle(Theme.text)
                    safetyBadge(candidate.safety)
                }
                Text(candidate.description)
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.textDim)
                    .lineLimit(2)
                Text(candidate.path)
                    .font(.pathCell)
                    .foregroundStyle(Theme.textFaint)
                    .lineLimit(1)
            }
            Spacer()
            VStack(alignment: .trailing, spacing: 2) {
                Text(humanBytes(candidate.allocatedSize))
                    .font(.dataCell)
                    .foregroundStyle(Theme.text)
                Text("\(candidate.fileCount.formatted()) files")
                    .font(.pathCell)
                    .foregroundStyle(Theme.textFaint)
            }
        }
        .padding(.vertical, 2)
        .contextMenu {
            Button("Finder에서 보기") {
                NSWorkspace.shared.activateFileViewerSelecting(
                    [URL(fileURLWithPath: candidate.path)])
            }
        }
    }

    private func adviceRow(_ advice: ToolAdviceInfo) -> some View {
        HStack(alignment: .center, spacing: 10) {
            Image(systemName: "terminal")
                .font(.system(size: 11))
                .foregroundStyle(advice.available ? Theme.accent : Theme.textFaint)
                .frame(width: 20)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(advice.tool)
                        .font(.system(size: 12.5, weight: .semibold))
                        .foregroundStyle(advice.available ? Theme.text : Theme.textFaint)
                    if !advice.available {
                        TagBadge(text: "사용 불가", color: Theme.textFaint)
                    }
                }
                HStack(spacing: 6) {
                    Text(advice.command)
                        .font(.pathCell)
                        .foregroundStyle(Theme.textDim)
                        .lineLimit(1)
                        .textSelection(.enabled)
                    Button {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(advice.command, forType: .string)
                        model.message = "복사됨: \(advice.command)"
                    } label: {
                        Image(systemName: "doc.on.doc")
                            .font(.system(size: 9))
                            .foregroundStyle(Theme.textFaint)
                    }
                    .buttonStyle(.plain)
                    .help("커맨드 복사")
                }
                Text("\(advice.description) — \(advice.detail)")
                    .font(.system(size: 10.5))
                    .foregroundStyle(Theme.textFaint)
                    .lineLimit(2)
            }
            Spacer()
            if let reclaim = advice.estimatedReclaim {
                VStack(alignment: .trailing, spacing: 1) {
                    Text(humanBytes(reclaim))
                        .font(.dataCell)
                        .foregroundStyle(Theme.accent)
                    Text("예상 회수")
                        .font(.system(size: 8, weight: .semibold))
                        .tracking(0.8)
                        .foregroundStyle(Theme.textFaint)
                }
            }
        }
        .padding(.vertical, 2)
    }

    private func safetyBadge(_ safety: String) -> some View {
        TagBadge(
            text: safety == "safe" ? "안전" : "검토 필요",
            color: safety == "safe" ? Theme.safe : Theme.warn
        )
    }

    private func categoryLabel(_ category: String) -> String {
        switch category {
        case "developer": return "개발 도구"
        case "package-manager": return "패키지 매니저 캐시"
        case "cache": return "앱 캐시"
        case "log": return "로그"
        case "ml-models": return "ML 모델"
        case "trash": return "휴지통 (Finder에서 비우기)"
        default: return category
        }
    }
}

/// 선택 요약 + 휴지통 이동/되돌리기 공용 하단 바 (계기판 리드아웃 스타일).
struct CartBar: View {
    let selectedCount: Int
    let selectedSize: UInt64
    let message: String?
    let undoAvailable: Bool
    let onTrash: () -> Void
    let onUndo: () -> Void
    let onRefresh: () -> Void

    var body: some View {
        HStack(spacing: 14) {
            if selectedCount > 0 {
                HStack(spacing: 8) {
                    InstrumentLabel(text: "선택")
                    Text("\(selectedCount)")
                        .font(.dataCell)
                        .foregroundStyle(Theme.text)
                    InstrumentLabel(text: "예상 회수")
                    Text(humanBytes(selectedSize))
                        .font(.dataCell)
                        .foregroundStyle(Theme.accent)
                }
            } else if let message {
                Text(message)
                    .font(.system(size: 12))
                    .monospacedDigit()
                    .foregroundStyle(Theme.textDim)
            } else {
                Text("정리할 항목을 선택하세요")
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.textFaint)
            }
            Spacer()
            Button {
                onRefresh()
            } label: {
                Image(systemName: "arrow.clockwise")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(Theme.textDim)
                    .padding(6)
            }
            .buttonStyle(.plain)
            .help("다시 검사")
            if undoAvailable {
                Button {
                    onUndo()
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
            Button {
                onTrash()
            } label: {
                HStack(spacing: 5) {
                    Image(systemName: "trash")
                        .font(.system(size: 10, weight: .bold))
                    Text("휴지통으로 이동")
                        .font(.system(size: 11, weight: .bold))
                }
                .foregroundStyle(selectedCount == 0 ? Theme.textFaint : Theme.bg)
                .padding(.horizontal, 12)
                .padding(.vertical, 6)
                .background(
                    selectedCount == 0 ? Theme.raised : Theme.accent,
                    in: RoundedRectangle(cornerRadius: 6)
                )
            }
            .buttonStyle(.plain)
            .disabled(selectedCount == 0)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(Theme.panel)
    }
}
