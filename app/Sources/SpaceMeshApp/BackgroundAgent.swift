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
            installLaunchAgent(settings)
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

    private func installLaunchAgent(_ settings: AppSettings) {
        let root = settings.watchedRoot
        let interval = settings.interval
        guard let cli = Self.cliPath() else {
            status = "CLI 바이너리를 찾을 수 없어 주기 모드를 설정하지 못했습니다"
            return
        }
        var arguments = [cli, root, "--db", AppModel.dbPath, "--depth", "0"]
        if settings.suggestEnabled {
            // 스냅샷과 함께 회수 제안도 계산해 파일로 남긴다 — 앱이 열릴 때 배너로 표시 (F6).
            arguments += [
                "--suggest",
                "--suggest-out", SuggestionStore.filePath,
                "--idle-days", "\(max(0, settings.suggestIdleDays))",
                "--suggest-min-mib", "\(Int(max(0, settings.suggestMinGiB) * 1024))",
            ]
        }
        let plist: [String: Any] = [
            "Label": launchAgentLabel,
            "ProgramArguments": arguments,
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
    /// 증분 재집계용 상주 핸들 — 첫 재집계는 전체 스캔으로 만들고 이후 refresh_paths.
    private var liveHandle: ScanHandle?
    /// FSEvents가 알려준 변경 디렉토리 (다음 재집계까지 누적).
    private var pendingPaths: Set<String> = []
    private var mustFullRescan = false
    /// 이보다 변경 경로가 많으면 증분 이득이 없다 — 전체 재스캔으로 강등.
    private let incrementalPathLimit = 256

    private func startLiveWatch(root: String, budgetGiB: Double) {
        watchRoot = root
        budgetBytes = budgetGiB > 0 ? UInt64(budgetGiB * 1_073_741_824) : 0
        liveHandle = nil
        pendingPaths = []
        mustFullRescan = false

        var context = FSEventStreamContext(
            version: 0,
            info: Unmanaged.passUnretained(self).toOpaque(),
            retain: nil, release: nil, copyDescription: nil)

        // latency 10초 — 이벤트를 커널이 모아서 배치로 전달(wakeup 최소화).
        // UseCFTypes: 콜백 eventPaths를 CFArray<String>으로 받아 변경 위치를 수집(F2 증분).
        let flags = UInt32(
            kFSEventStreamCreateFlagNoDefer | kFSEventStreamCreateFlagWatchRoot
                | kFSEventStreamCreateFlagUseCFTypes)
        guard
            let stream = FSEventStreamCreate(
                kCFAllocatorDefault,
                { _, info, numEvents, eventPaths, eventFlags, _ in
                    guard let info else { return }
                    let agent = Unmanaged<BackgroundAgent>.fromOpaque(info)
                        .takeUnretainedValue()
                    let paths =
                        (Unmanaged<CFArray>.fromOpaque(eventPaths).takeUnretainedValue()
                            as NSArray as? [String]) ?? []
                    // 커널이 이벤트를 합쳤으면(큐 넘침 등) 하위 전체를 다시 봐야 한다.
                    var coalesced = false
                    for i in 0..<numEvents
                    where eventFlags[i] & UInt32(kFSEventStreamEventFlagMustScanSubDirs) != 0 {
                        coalesced = true
                    }
                    let mustFull = coalesced
                    Task { @MainActor in agent.onFSEvents(paths: paths, mustFull: mustFull) }
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
        liveHandle = nil
        pendingPaths = []
        mustFullRescan = false
    }

    /// 이벤트 수신 — 즉시 재집계하지 않고 debounce. IO 폭주 시 오히려 배치가 커져 효율적.
    /// 변경 경로를 누적해 두면 재집계가 서브트리 단위 증분으로 동작한다 (F2).
    private func onFSEvents(paths: [String], mustFull: Bool) {
        if pendingSince == nil { pendingSince = Date() }
        if mustFull { mustFullRescan = true }
        pendingPaths.formUnion(paths)
        if pendingPaths.count > incrementalPathLimit { mustFullRescan = true }
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
        let changed = Array(pendingPaths)
        let incremental = liveHandle != nil && !mustFullRescan && !changed.isEmpty
        pendingPaths = []
        mustFullRescan = false

        let handle: ScanHandle?
        var mode = "전체"
        if incremental, let live = liveHandle {
            // 변경된 서브트리만 재스캔하고 갱신된 트리를 스냅샷으로 저장 (F2).
            let ok = await Task.detached(priority: .background) { () -> Bool in
                guard (try? live.refreshPaths(absPaths: changed, minFileMib: 50)) != nil
                else { return false }
                return (try? live.saveToDb(dbPath: AppModel.dbPath)) != nil
            }.value
            handle = ok ? live : nil
            mode = "증분 \(changed.count)곳"
        } else {
            handle = try? await Task.detached(priority: .background) {
                // 스냅샷도 저장해 '변화' 탭에 축적.
                try scanAndSave(path: root, minFileMib: 50, dbPath: AppModel.dbPath)
            }.value
        }
        isRecomputing = false
        guard let handle else {
            // 증분 실패(루트 소실 등)면 다음 재집계는 전체 스캔으로.
            liveHandle = nil
            status = "재집계 실패"
            return
        }
        liveHandle = handle
        let total = (try? handle.nodeAt(indexPath: []).allocatedSize) ?? 0
        lastTotal = total
        lastUpdated = Date()
        status = "최근 집계 \(humanBytes(total)) (\(mode)) · \(shortNow())"

        if budgetBytes > 0 && total > budgetBytes {
            notifyBudget(total: total, budget: budgetBytes, root: root)
        }

        // 정책 기반 회수 제안 (F6) — 평가는 6시간 스로틀, 알림은 24시간 중복 방지.
        let settings = AppSettings.shared
        if settings.suggestEnabled {
            if let suggestion = await SuggestionStore.shared.evaluate(
                handle: handle, root: root, settings: settings)
            {
                SuggestionStore.shared.notifyIfNew(suggestion)
            }
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
