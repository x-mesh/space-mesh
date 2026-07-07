import AppKit
import QuickLook
import SpaceMeshCore
import SwiftUI

/// 중복 파일 탐지 뷰 — 크기→부분해시→전체해시 3단 필터 결과.
struct DuplicatesView: View {
    @ObservedObject var model: CleanupModel
    @ObservedObject var plan: ReclaimPlan
    let defaultRoot: String

    @State private var root: String = ""
    @State private var minMib: UInt64 = 10
    @State private var confirmTrash = false
    @State private var previewURL: URL?

    private var selectedItems: [(path: String, size: UInt64)] {
        model.dupGroups.flatMap { group -> [(String, UInt64)] in
            // 항목당 가격은 클론 보정된 그룹 회수 가능량을 잠재 victim 수로 나눈 값 —
            // 이미 클론인 그룹(회수 0)을 fileSize 전액으로 계산해 예상치를
            // 부풀리지 않는다 (헤더의 보정 표시와 일관).
            let perVictim =
                group.files.count > 1
                ? group.reclaimable / UInt64(group.files.count - 1)
                : 0
            return group.files
                .filter { model.selectedDupPaths.contains($0) }
                .map { ($0, perVictim) }
        }
    }
    private var selectedSize: UInt64 {
        selectedItems.reduce(0) { $0 + $1.size }
    }
    private var totalReclaimable: UInt64 {
        model.dupGroups.reduce(0) { $0 + $1.reclaimable }
    }

    var body: some View {
        VStack(spacing: 0) {
            searchBar
            Divider()
            if model.isFindingDups {
                ScanningView(startedAt: model.dupStartedAt, label: "중복 검사 중", unit: "hashed")
            } else if model.dupGroups.isEmpty {
                VStack(spacing: 10) {
                    Image(systemName: "doc.on.doc")
                        .font(.system(size: 40))
                        .foregroundStyle(.secondary)
                    Text(model.dupSearched ? "중복 파일이 없습니다" : "검사할 경로와 최소 크기를 정하고 검색을 누르세요")
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                groupList
            }
            Divider()
            CartBar(
                selectedCount: selectedItems.count,
                selectedSize: selectedSize,
                message: model.message,
                undoAvailable: !model.lastBatch.isEmpty,
                onTrash: { confirmTrash = true },
                onUndo: { model.undoLastBatch() },
                onRefresh: { model.findDups(root: root, minMib: minMib) },
                onAddToPlan: {
                    plan.add(
                        selectedItems.map {
                            PlanItem(duplicatePath: $0.path, estimated: $0.size)
                        })
                    model.selectedDupPaths = []
                }
            )
        }
        .onAppear {
            if root.isEmpty { root = defaultRoot }
        }
        .quickLookPreview($previewURL)
        .confirmationDialog(
            "\(selectedItems.count)개 중복 파일 (\(humanBytes(selectedSize)))을 휴지통으로 이동할까요?",
            isPresented: $confirmTrash, titleVisibility: .visible
        ) {
            Button("휴지통으로 이동", role: .destructive) {
                model.trash(paths: selectedItems)
                model.findDups(root: root, minMib: minMib)
            }
        }
    }

    private var searchBar: some View {
        HStack(spacing: 10) {
            HStack(spacing: 6) {
                Image(systemName: "doc.on.doc")
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.textFaint)
                TextField("검사할 경로", text: $root)
                    .textFieldStyle(.plain)
                    .font(.pathCell)
                    .foregroundStyle(Theme.text)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .background(Theme.raised, in: RoundedRectangle(cornerRadius: 6))
            .frame(maxWidth: 320)
            .disabled(model.isFindingDups)

            HStack(spacing: 4) {
                InstrumentLabel(text: "최소")
                TextField("MiB", value: $minMib, format: .number)
                    .textFieldStyle(.plain)
                    .font(.dataCell)
                    .foregroundStyle(Theme.text)
                    .frame(width: 40)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 4)
                    .background(Theme.raised, in: RoundedRectangle(cornerRadius: 5))
                InstrumentLabel(text: "MiB")
            }

            Button {
                model.findDups(root: root, minMib: minMib)
            } label: {
                Text("검색")
                    .font(.system(size: 11, weight: .bold))
                    .foregroundStyle(model.isFindingDups || root.isEmpty ? Theme.textFaint : Theme.bg)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 6)
                    .background(
                        model.isFindingDups || root.isEmpty ? Theme.raised : Theme.accent,
                        in: RoundedRectangle(cornerRadius: 6)
                    )
            }
            .buttonStyle(.plain)
            .disabled(model.isFindingDups || root.isEmpty)

            Spacer()

            if !model.dupGroups.isEmpty {
                InstrumentLabel(text: "\(model.dupGroups.count)그룹")
                Text(humanBytes(totalReclaimable))
                    .font(.dataCell)
                    .foregroundStyle(Theme.accent)
                Button("첫 파일만 남기고 모두 선택") {
                    model.selectedDupPaths = Set(
                        model.dupGroups.flatMap { $0.files.dropFirst() })
                }
                .font(.system(size: 11, weight: .semibold))
                .buttonStyle(.plain)
                .foregroundStyle(Theme.accent)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(Theme.panel)
    }

    private var groupList: some View {
        List {
            ForEach(model.dupGroups, id: \.hashHex) { group in
                Section {
                    ForEach(Array(group.files.enumerated()), id: \.element) { i, path in
                        dupRow(path: path, isFirst: i == 0, size: group.fileSize)
                            .listRowBackground(Theme.bg)
                    }
                } header: {
                    HStack(spacing: 8) {
                        Text("\(group.files.count) × \(humanBytes(group.fileSize))")
                            .font(.dataCell)
                            .foregroundStyle(Theme.text)
                        InstrumentLabel(text: "회수 가능")
                        Text(humanBytes(group.reclaimable))
                            .font(.dataCell)
                            .foregroundStyle(group.reclaimable == 0 ? Theme.textFaint : Theme.accent)
                        if group.cloneShared {
                            TagBadge(text: "일부 클론 — 회수 보정됨", color: Theme.info)
                                .help("이미 APFS 클론으로 블록을 공유하는 파일이 있어, 지워도 그만큼은 공간이 늘지 않습니다")
                        }
                        Spacer()
                        if group.reclaimable > 0 {
                            Button {
                                model.mergeGroupAsClones(group) {
                                    model.findDups(root: root, minMib: minMib)
                                }
                            } label: {
                                HStack(spacing: 4) {
                                    Image(systemName: "arrow.triangle.merge")
                                        .font(.system(size: 9, weight: .bold))
                                    Text("클론으로 병합 (무손실)")
                                        .font(.system(size: 10.5, weight: .semibold))
                                }
                                .foregroundStyle(model.isMerging ? Theme.textFaint : Theme.safe)
                            }
                            .buttonStyle(.plain)
                            .disabled(model.isMerging)
                            .help("첫 파일만 실블록을 갖고 나머지를 APFS 클론 사본으로 교체합니다 — 모든 파일이 그대로 남고 공간만 회수됩니다")
                        }
                    }
                    .textCase(nil)
                }
            }
        }
        .listStyle(.inset)
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
    }

    private func dupRow(path: String, isFirst: Bool, size: UInt64) -> some View {
        HStack(spacing: 10) {
            Toggle(
                "",
                isOn: Binding(
                    get: { model.selectedDupPaths.contains(path) },
                    set: { on in
                        if on {
                            model.selectedDupPaths.insert(path)
                        } else {
                            model.selectedDupPaths.remove(path)
                        }
                    }
                )
            )
            .labelsHidden()
            VStack(alignment: .leading, spacing: 1) {
                Text((path as NSString).lastPathComponent)
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                Text((path as NSString).deletingLastPathComponent)
                    .font(.pathCell)
                    .foregroundStyle(Theme.textFaint)
                    .lineLimit(1)
            }
            if isFirst {
                TagBadge(text: "원본 후보", color: Theme.info)
            }
            Spacer()
        }
        .contentShape(Rectangle())
        .onTapGesture(count: 2) {
            previewURL = URL(fileURLWithPath: path)
        }
        .contextMenu {
            Button("Quick Look") { previewURL = URL(fileURLWithPath: path) }
            Button("Finder에서 보기") {
                NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
            }
        }
    }
}
