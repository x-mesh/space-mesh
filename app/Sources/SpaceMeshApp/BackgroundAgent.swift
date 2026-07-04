import AppKit
import CoreServices
import Foundation
import SpaceMeshCore
import UserNotifications

/// 주기 스냅샷(launchd)과 실시간 감시(FSEvents)를 관리한다.
/// 설계 원칙: IO가 많은 맥에서도 부하를 낮게 — 이벤트 즉시반응 금지(debounce),
/// FSEvents latency 크게, 유휴/전원 게이팅, 저전력 IO.
@MainActor
final class BackgroundAgent: ObservableObject {
    static let shared = BackgroundAgent()

    /// 실시간 모드에서 마지막으로 집계한 총 allocated (메뉴바 표시용).
    @Published var lastTotal: UInt64?
    @Published var lastUpdated: Date?
    @Published var isRecomputing = false
    @Published var status: String = ""

    private var eventStream: FSEventStreamRef?
    private var debounceTask: Task<Void, Never>?
    private var pendingSince: Date?

    private let launchAgentLabel = "com.spacemesh.periodic"

    // MARK: - 모드 적용

    /// 현재 설정에 맞춰 에이전트 상태를 맞춘다. 설정 변경 시마다 호출.
    func apply(_ settings: AppSettings) {
        stopLiveWatch()
        removeLaunchAgent()  // 항상 깨끗이 지우고 필요한 것만 다시 건다.

        switch settings.mode {
        case .off:
            status = "감시 꺼짐"
        case .periodic:
            installLaunchAgent(root: settings.watchedRoot, interval: settings.interval)
        case .live:
            requestNotificationAuth()
            startLiveWatch(root: settings.watchedRoot, budgetGiB: settings.budgetGiB)
        }
    }

    // MARK: - 주기 스냅샷 (launchd LaunchAgent)

    private var launchAgentPath: String {
        (NSHomeDirectory() as NSString)
            .appendingPathComponent("Library/LaunchAgents/\(launchAgentLabel).plist")
    }

    /// 번들에 동봉된(또는 개발 빌드의) CLI 바이너리 경로.
    nonisolated static func cliPath() -> String? {
        // 배포 시: 앱 번들 Resources. 개발 시: core/target/release.
        if let bundled = Bundle.main.path(forResource: "space-mesh", ofType: nil) {
            return bundled
        }
        let devPath = URL(fileURLWithPath: #filePath)  // .../app/Sources/SpaceMeshApp/BackgroundAgent.swift
            .deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()  // → space-mesh/
            .appendingPathComponent("core/target/release/space-mesh")
        return FileManager.default.isExecutableFile(atPath: devPath.path) ? devPath.path : nil
    }

    private func installLaunchAgent(root: String, interval: PeriodicInterval) {
        guard let cli = Self.cliPath() else {
            status = "CLI 바이너리를 찾을 수 없어 주기 모드를 설정하지 못했습니다"
            return
        }
        let plist: [String: Any] = [
            "Label": launchAgentLabel,
            "ProgramArguments": [cli, root, "--db", AppModel.dbPath, "--depth", "0"],
            "StartInterval": interval.seconds,
            "ProcessType": "Background",  // 저우선순위 스케줄링
            "LowPriorityIO": true,  // 다른 IO를 방해하지 않음
            "Nice": 10,
            "RunAtLoad": false,
        ]
        do {
            let data = try PropertyListSerialization.data(
                fromPropertyList: plist, format: .xml, options: 0)
            try data.write(to: URL(fileURLWithPath: launchAgentPath))
            bootLaunchAgent(load: true)
            status =
                "주기 스냅샷 등록됨 (\(interval.title), 저전력 IO) — \(URL(fileURLWithPath: root).lastPathComponent)"
        } catch {
            status = "LaunchAgent 작성 실패: \(error.localizedDescription)"
        }
    }

    private func removeLaunchAgent() {
        guard FileManager.default.fileExists(atPath: launchAgentPath) else { return }
        bootLaunchAgent(load: false)
        try? FileManager.default.removeItem(atPath: launchAgentPath)
    }

    /// launchctl bootstrap/bootout (현대 방식) — 실패 시 무시(이미 로드/미로드).
    private func bootLaunchAgent(load: Bool) {
        let uid = getuid()
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        if load {
            proc.arguments = ["bootstrap", "gui/\(uid)", launchAgentPath]
        } else {
            proc.arguments = ["bootout", "gui/\(uid)/\(launchAgentLabel)"]
        }
        proc.standardOutput = FileHandle.nullDevice
        proc.standardError = FileHandle.nullDevice
        try? proc.run()
        proc.waitUntilExit()
    }

    // MARK: - 실시간 감시 (FSEvents)

    private var watchRoot = ""
    private var budgetBytes: UInt64 = 0

    private func startLiveWatch(root: String, budgetGiB: Double) {
        watchRoot = root
        budgetBytes = budgetGiB > 0 ? UInt64(budgetGiB * 1_073_741_824) : 0

        var context = FSEventStreamContext(
            version: 0,
            info: Unmanaged.passUnretained(self).toOpaque(),
            retain: nil, release: nil, copyDescription: nil)

        // latency 10초 — 이벤트를 커널이 모아서 배치로 전달(wakeup 최소화).
        let flags = UInt32(
            kFSEventStreamCreateFlagNoDefer | kFSEventStreamCreateFlagWatchRoot)
        guard
            let stream = FSEventStreamCreate(
                kCFAllocatorDefault,
                { _, info, _, _, _, _ in
                    guard let info else { return }
                    let agent = Unmanaged<BackgroundAgent>.fromOpaque(info)
                        .takeUnretainedValue()
                    Task { @MainActor in agent.onFSEvents() }
                },
                &context,
                [root] as CFArray,
                FSEventStreamEventId(kFSEventStreamEventIdSinceNow),
                10.0,  // latency 초
                flags)
        else {
            status = "FSEvents 스트림 생성 실패"
            return
        }
        // 저우선순위 백그라운드 큐에서 처리.
        FSEventStreamSetDispatchQueue(stream, DispatchQueue.global(qos: .background))
        FSEventStreamStart(stream)
        eventStream = stream
        status = "실시간 감시 중 — \(URL(fileURLWithPath: root).lastPathComponent)"
        // 시작 시 1회 기준값 확보.
        scheduleRecompute(delay: 2)
    }

    private func stopLiveWatch() {
        if let stream = eventStream {
            FSEventStreamStop(stream)
            FSEventStreamInvalidate(stream)
            FSEventStreamRelease(stream)
            eventStream = nil
        }
        debounceTask?.cancel()
        debounceTask = nil
        pendingSince = nil
    }

    /// 이벤트 수신 — 즉시 재집계하지 않고 debounce. IO 폭주 시 오히려 배치가 커져 효율적.
    private func onFSEvents() {
        if pendingSince == nil { pendingSince = Date() }
        scheduleRecompute(delay: 8)
    }

    /// debounce + 유휴/전원 게이팅 후 배치 재집계.
    private func scheduleRecompute(delay: UInt64) {
        debounceTask?.cancel()
        debounceTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(Double(delay)))
            guard let self, !Task.isCancelled else { return }
            // 발열 심하면 미룬다(사용자가 무거운 작업 중일 가능성).
            if ProcessInfo.processInfo.thermalState == .serious
                || ProcessInfo.processInfo.thermalState == .critical
            {
                self.status = "발열 감지 — 재집계 지연 중"
                self.scheduleRecompute(delay: 60)
                return
            }
            await self.recompute()
        }
    }

    private func recompute() async {
        guard !watchRoot.isEmpty else { return }
        isRecomputing = true
        pendingSince = nil
        let root = watchRoot
        let handle = try? await Task.detached(priority: .background) {
            // 스냅샷도 저장해 '변화' 탭에 축적.
            try scanAndSave(path: root, minFileMib: 50, dbPath: AppModel.dbPath)
        }.value
        isRecomputing = false
        guard let handle else {
            status = "재집계 실패"
            return
        }
        let total = (try? handle.nodeAt(indexPath: []).allocatedSize) ?? 0
        lastTotal = total
        lastUpdated = Date()
        status = "최근 집계 \(humanBytes(total)) · \(shortNow())"

        if budgetBytes > 0 && total > budgetBytes {
            notifyBudget(total: total, budget: budgetBytes, root: root)
        }
    }

    // MARK: - 알림

    private func requestNotificationAuth() {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) {
            _, _ in
        }
    }

    private func notifyBudget(total: UInt64, budget: UInt64, root: String) {
        let content = UNMutableNotificationContent()
        content.title = "디스크 예산 초과"
        content.body =
            "\(URL(fileURLWithPath: root).lastPathComponent): \(humanBytes(total)) / 예산 \(humanBytes(budget))"
        content.sound = .default
        let req = UNNotificationRequest(
            identifier: "budget-\(root)", content: content, trigger: nil)
        UNUserNotificationCenter.current().add(req)
    }

    private func shortNow() -> String {
        let f = DateFormatter()
        f.dateFormat = "HH:mm"
        return f.string(from: Date())
    }
}
