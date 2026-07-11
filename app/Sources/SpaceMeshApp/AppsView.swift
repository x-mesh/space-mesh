import AppKit
import SpaceMeshCore
import SwiftUI

/// 설치된 앱을 크기순으로 세우고 "마지막으로 쓴 날"을 붙여 보여준다.
///
/// 기본 스캔 루트가 홈이라 /Applications(실측 46 GiB)는 트리맵에 잡히지 않는다.
/// 여기서만 볼 수 있는 화면이라, 스캔을 하지 않았어도 열린다.
///
/// 안전 불변식:
///   - 휴지통으로 보내는 건 **번들뿐**이다. 앱 데이터(~/Library)는 크기만 보여주고 건드리지 않는다.
///   - Apple 시스템 앱은 아예 고를 수 없다.
///   - "기록 없음"과 "180일+ 미사용"은 **다른 배지**다. 둘을 묶으면 매일 쓰는 앱을 지우라고 권하게 된다.
struct AppsView: View {
    @ObservedObject var model: CleanupModel

    /// 사이드바 "미수정 180일+"와 같은 기준을 쓴다 — 제품 안에서 "안 쓴다"의 정의는 하나여야 한다.
    private static let idleThresholdDays: UInt64 = 180

    @State private var apps: [AppInfo] = []
    @State private var selected: Set<String> = []
    @State private var isLoading = false
    @State private var loaded = false
    @State private var confirmTrash = false
    /// bundleId → 전용 데이터 크기. 목록 전체를 미리 계산하면 4초가 더 들어(번들 스캔보다 비싸다)
    /// 고른 앱만 그때 계산한다.
    @State private var dataSizes: [String: UInt64] = [:]

    private var selectedApps: [AppInfo] {
        apps.filter { selected.contains($0.path) }
    }
    private var selectedSize: UInt64 {
        selectedApps.reduce(0) { $0 + $1.allocatedSize }
    }
    private var totalSize: UInt64 {
        apps.reduce(0) { $0 + $1.allocatedSize }
    }
    private var idleApps: [AppInfo] {
        apps.filter { isIdle($0) }
    }

    /// 180일 넘게 안 쓴 앱. **기록이 없는 앱은 여기 들어오지 않는다** — 모르는 것과 안 쓰는 것은 다르다.
    private func isIdle(_ app: AppInfo) -> Bool {
        guard let days = app.lastUsedDays else { return false }
        return days >= Self.idleThresholdDays && !app.isApple
    }

    var body: some View {
        VStack(spacing: 0) {
            if isLoading {
                VStack(spacing: 12) {
                    ProgressView().controlSize(.large)
                    InstrumentLabel(text: "앱 훑는 중")
                    Text("/Applications 45 GiB를 실제로 재는 중입니다")
                        .font(.pathCell)
                        .foregroundStyle(Theme.textFaint)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if apps.isEmpty {
                VStack(spacing: 10) {
                    Image(systemName: "square.grid.2x2")
                        .font(.system(size: 40))
                        .foregroundStyle(.secondary)
                    Text("/Applications에 앱이 없습니다").foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                summaryHeader
                Divider()
                appList
            }
            Divider()
            CartBar(
                selectedCount: selectedApps.count,
                selectedSize: selectedSize,
                message: model.message,
                undoAvailable: !model.lastBatch.isEmpty,
                onTrash: { confirmTrash = true },
                onUndo: { model.undoLastBatch() },
                onRefresh: { reload() }
            )
        }
        .onAppear { if !loaded { reload() } }
        .confirmationDialog(
            "\(selectedApps.count)개 앱 (\(humanBytes(selectedSize)))을 휴지통으로 옮길까요?",
            isPresented: $confirmTrash, titleVisibility: .visible
        ) {
            Button("휴지통으로 이동", role: .destructive) { trashSelected() }
        } message: {
            Text("앱 번들만 옮깁니다. 설정·문서 같은 앱 데이터는 그대로 둡니다.")
        }
    }

    // MARK: - 로드

    private func reload() {
        isLoading = true
        selected = []
        Task {
            let found = await Task.detached { listApps() }.value
            self.apps = found.sorted { $0.allocatedSize > $1.allocatedSize }
            self.dataSizes = [:]
            self.isLoading = false
            self.loaded = true
        }
    }

    /// 고른 앱의 데이터 크기만 뒤늦게 채운다.
    private func loadDataSize(for app: AppInfo) {
        guard !app.bundleId.isEmpty, dataSizes[app.bundleId] == nil else { return }
        Task {
            let size = await Task.detached { appDataSize(bundleId: app.bundleId) }.value
            self.dataSizes[app.bundleId] = size
        }
    }

    /// **번들 경로만** 휴지통으로 보낸다. 앱 데이터 경로는 절대 넣지 않는다.
    private func trashSelected() {
        let bundles = selectedApps.map { (path: $0.path, size: $0.allocatedSize) }
        _ = model.trash(paths: bundles)
        let moved = Set(bundles.map(\.path))
        apps.removeAll { moved.contains($0.path) }
        selected = []
    }

    // MARK: - 헤더

    private var summaryHeader: some View {
        HStack(spacing: 10) {
            InstrumentLabel(text: "\(apps.count)개 앱")
            Text(humanBytes(totalSize))
                .font(.dataCell)
                .foregroundStyle(Theme.accent)
            if !idleApps.isEmpty {
                InstrumentLabel(text: "180일+ 미사용 \(idleApps.count)개")
                Text(humanBytes(idleApps.reduce(0) { $0 + $1.allocatedSize }))
                    .font(.dataCell)
                    .foregroundStyle(Theme.warn)
            }
            Spacer()
            Button("180일+ 미사용 모두 선택") {
                selected = Set(idleApps.map(\.path))
            }
            .font(.system(size: 11, weight: .semibold))
            .buttonStyle(.plain)
            .foregroundStyle(Theme.accent)
            .disabled(idleApps.isEmpty)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .background(Theme.panel)
    }

    // MARK: - 목록

    private var appList: some View {
        List {
            ForEach(apps, id: \.path) { app in
                appRow(app)
                    .listRowBackground(Theme.bg)
            }
        }
        .listStyle(.inset)
        .scrollContentBackground(.hidden)
        .background(Theme.bg)
    }

    private func appRow(_ app: AppInfo) -> some View {
        HStack(spacing: 10) {
            Toggle(
                "",
                isOn: Binding(
                    get: { selected.contains(app.path) },
                    set: { on in
                        if on {
                            selected.insert(app.path)
                            loadDataSize(for: app)
                        } else {
                            selected.remove(app.path)
                        }
                    }
                )
            )
            .labelsHidden()
            // Apple 시스템 앱은 고를 수 없다 — 지우면 안 되는 것을 장바구니에 못 담게 한다.
            .disabled(app.isApple)

            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 6) {
                    Text(app.name)
                        .font(.system(size: 12.5, weight: .semibold))
                        .foregroundStyle(app.isApple ? Theme.textDim : Theme.text)
                        .lineLimit(1)
                    if app.isApple {
                        Image(systemName: "lock.fill")
                            .font(.system(size: 9))
                            .foregroundStyle(Theme.textFaint)
                            .help(protectionReason(app))
                    }
                }
                Text(lastUsedText(app))
                    .font(.pathCell)
                    .foregroundStyle(Theme.textFaint)
            }

            usageBadge(app)
            sourceBadge(app)

            Spacer()

            VStack(alignment: .trailing, spacing: 1) {
                Text(humanBytes(app.allocatedSize))
                    .font(.dataCell)
                    .foregroundStyle(Theme.text)
                // 데이터 크기는 고른 앱만 계산된다. 보존된다는 걸 분명히 적는다.
                if let data = dataSizes[app.bundleId], data > 0 {
                    Text("데이터 \(humanBytes(data)) · 보존")
                        .font(.pathCell)
                        .foregroundStyle(Theme.safe)
                        .help("앱 데이터는 지우지 않습니다 — 재설치하면 설정이 그대로 살아납니다")
                }
            }
        }
        .contextMenu {
            Button("Finder에서 보기") {
                NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: app.path)])
            }
            if !app.recreateCommand.isEmpty {
                Button("복원 명령 복사") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(app.recreateCommand, forType: .string)
                }
            }
        }
    }

    /// 왜 잠겼는지 정직하게 말한다. 번들 id가 없는 앱(Info.plist가 깨진 경우)까지
    /// "macOS 시스템 앱"이라고 하면 거짓말이 된다 — 정체를 모를 뿐이다.
    private func protectionReason(_ app: AppInfo) -> String {
        app.bundleId.isEmpty
            ? "정체를 확인할 수 없어 잠갔습니다 (Info.plist를 읽지 못했습니다) — 지우려면 Finder에서 직접 옮기세요"
            : "macOS 시스템 앱 — 삭제할 수 없습니다"
    }

    private func lastUsedText(_ app: AppInfo) -> String {
        guard let days = app.lastUsedDays else {
            return "사용 기록 없음"
        }
        switch days {
        case 0: return "오늘 사용"
        case 1: return "어제 사용"
        default: return "\(days)일 전 사용"
        }
    }

    /// 사용 상태 배지. **"기록 없음"은 "미사용"과 다른 배지다** —
    /// 같은 배지로 묶으면 기록이 안 남는 앱(실측 29%, Xcode 포함)을 지우라고 권하게 된다.
    @ViewBuilder
    private func usageBadge(_ app: AppInfo) -> some View {
        if let days = app.lastUsedDays {
            if days >= Self.idleThresholdDays {
                TagBadge(text: "유휴 \(days / 30)개월", color: Theme.warn, icon: "moon.zzz.fill")
                    .help("\(days)일째 실행하지 않았습니다")
            }
        } else {
            TagBadge(text: "기록 없음", color: Theme.textFaint)
                .help("사용 기록을 찾지 못했습니다 — 안 쓴다는 뜻이 아닙니다")
        }
    }

    /// 복원 방법을 아는지. 모르면 경고를 띄운다 — 지운 뒤 되돌릴 길이 없다는 뜻이기 때문이다.
    @ViewBuilder
    private func sourceBadge(_ app: AppInfo) -> some View {
        switch app.source {
        case "brew":
            TagBadge(text: "brew", color: Theme.safe, icon: "checkmark.seal.fill")
                .help(app.recreateCommand)
        case "mas":
            TagBadge(text: "App Store", color: Theme.safe, icon: "checkmark.seal.fill")
                .help("App Store에서 다시 받을 수 있습니다")
        default:
            TagBadge(text: "복원 명령 불명", color: Theme.warn)
                .help("직접 내려받은 앱입니다 — 지우면 다시 받을 방법을 직접 찾아야 합니다")
        }
    }
}
