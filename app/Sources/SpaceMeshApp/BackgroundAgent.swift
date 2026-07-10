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
            growthAlertBytes =
                settings.growthAlertGiB > 0
                ? UInt64(settings.growthAlertGiB * 1_073_741_824) : 0
            startLiveWatch(root: settings.watchedRoot, budgetGiB: settings.budgetGiB)
        }
    }

    /// Growth Watch 임계 (bytes). 0이면 끔.
    private var growthAlertBytes: UInt64 = 0
    /// 마지막으로 성장 알림을 보낸 스냅샷 id — 같은 스냅샷 쌍으로 중복 알림 방지.
    private var lastGrowthNotifiedSnapId: Int64 = 0

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

    // MARK: 증분 재집계 상태 (M4)

    /// 마지막 재집계의 핸들 — 증분 rescan의 베이스. nil이면 풀스캔.
    private var lastHandle: ScanHandle?
    /// debounce 창 동안 누적된 변경 디렉토리 경로 (FSEvents 기본 = 디렉토리 단위).
    private var pendingPaths: Set<String> = []
    /// pendingPaths에 포함된 이벤트 중 가장 큰 FSEvents event id.
    private var pendingEventCursor: UInt64 = 0
    /// 비동기 스냅샷 로드가 stop/restart 뒤 늦게 stream을 시작하지 못하게 한다.
    private var streamGeneration: UInt64 = 0
    /// MustScanSubDirs(드롭 동반)/RootChanged/Mount/Unmount 감지 — 다음 재집계는 풀스캔.
    private var forceFullScan = false
    /// 재베이스 정책: 증분 100회 또는 24시간마다 풀스캔 (Syncthing/Watchman 선례).
    private var incrementalCount = 0
    private var lastFullScanAt = Date.distantPast
    private static let rebaseEveryIncrements = 100
    private static let rebaseInterval: TimeInterval = 24 * 3600

    /// 풀스캔 강등 신호 플래그 (Apple 가이드: MustScanSubDirs만 봐도 드롭 케이스 커버).
    private static let degradeFlags: FSEventStreamEventFlags = FSEventStreamEventFlags(
        kFSEventStreamEventFlagMustScanSubDirs | kFSEventStreamEventFlagRootChanged
            | kFSEventStreamEventFlagMount | kFSEventStreamEventFlagUnmount
            | kFSEventStreamEventFlagKernelDropped | kFSEventStreamEventFlagUserDropped
            | kFSEventStreamEventFlagEventIdsWrapped)

    private func startLiveWatch(root: String, budgetGiB: Double) {
        watchRoot = root
        budgetBytes = budgetGiB > 0 ? UInt64(budgetGiB * 1_073_741_824) : 0
        // 메모리 상태를 비운 뒤 DB 스냅샷에서 증분 베이스를 비동기로 복원한다.
        lastHandle = nil
        pendingPaths = []
        pendingEventCursor = 0
        forceFullScan = false
        incrementalCount = 0
        lastFullScanAt = .distantPast
        streamGeneration &+= 1
        let generation = streamGeneration
        status = "이전 스캔 상태 확인 중"

        Task {
            let state = await Task.detached(priority: .utility) {
                try? loadSnapshotState(dbPath: AppModel.dbPath, rootPath: root)
            }.value
            guard self.streamGeneration == generation, self.watchRoot == root else { return }
            self.beginLiveWatch(root: root, restored: state)
        }
    }

    private func beginLiveWatch(root: String, restored: SnapshotState?) {
        var since = FSEventStreamEventId(kFSEventStreamEventIdSinceNow)
        if let restored, restored.incrementalReady, restored.fseventCursor > 0 {
            lastHandle = restored.handle
            since = FSEventStreamEventId(restored.fseventCursor)
            lastFullScanAt = ISO8601DateFormatter().date(from: restored.createdAt) ?? .distantPast
            let total = (try? restored.handle.nodeAt(indexPath: []).allocatedSize) ?? 0
            lastTotal = total
            lastUpdated = lastFullScanAt == .distantPast ? nil : lastFullScanAt
            status = "스냅샷 복원 · \(humanBytes(total))"
        }

        var context = FSEventStreamContext(
            version: 0,
            info: Unmanaged.passUnretained(self).toOpaque(),
            retain: nil, release: nil, copyDescription: nil)

        // latency 10초 — 이벤트를 커널이 모아서 배치로 전달(wakeup 최소화).
        // UseCFTypes: 콜백의 eventPaths를 CFArray<CFString>으로 받아 증분 대상 수집.
        let flags = UInt32(
            kFSEventStreamCreateFlagNoDefer | kFSEventStreamCreateFlagWatchRoot
                | kFSEventStreamCreateFlagUseCFTypes | kFSEventStreamCreateFlagIgnoreSelf)
        guard
            let stream = FSEventStreamCreate(
                kCFAllocatorDefault,
                { _, info, numEvents, eventPaths, eventFlags, eventIds in
                    guard let info else { return }
                    let agent = Unmanaged<BackgroundAgent>.fromOpaque(info)
                        .takeUnretainedValue()
                    // UseCFTypes → CFArray<CFString>. 실패 시 빈 배열(풀스캔 경로).
                    let paths =
                        Unmanaged<CFArray>.fromOpaque(eventPaths).takeUnretainedValue()
                        as? [String] ?? []
                    let flagsArr = Array(UnsafeBufferPointer(start: eventFlags, count: numEvents))
                    let ids = Array(UnsafeBufferPointer(start: eventIds, count: numEvents))
                    Task { @MainActor in
                        agent.onFSEvents(paths: paths, flags: flagsArr, eventIds: ids)
                    }
                },
                &context,
                [root] as CFArray,
                since,
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
        if lastHandle == nil {
            status = "실시간 감시 중 — 기준 스캔 준비"
            scheduleRecompute(delay: 2)
        } else if Date().timeIntervalSince(lastFullScanAt) >= Self.rebaseInterval {
            scheduleRecompute(delay: 2)
        }
    }

    private func stopLiveWatch() {
        streamGeneration &+= 1
        if let stream = eventStream {
            FSEventStreamStop(stream)
            FSEventStreamInvalidate(stream)
            FSEventStreamRelease(stream)
            eventStream = nil
        }
        debounceTask?.cancel()
        debounceTask = nil
        pendingSince = nil
        // 증분 베이스 해제 — live 모드 밖에서는 트리를 상주시키지 않는다.
        lastHandle = nil
        pendingPaths = []
        pendingEventCursor = 0
        isRecomputing = false
    }

    /// 이벤트 수신 — 즉시 재집계하지 않고 debounce. IO 폭주 시 오히려 배치가 커져 효율적.
    /// 변경 디렉토리 경로를 누적하고, 위험 플래그는 풀스캔 강등으로 표시한다 (M4).
    private func onFSEvents(
        paths: [String], flags: [FSEventStreamEventFlags],
        eventIds: [FSEventStreamEventId]
    ) {
        for (i, path) in paths.enumerated() {
            if i < flags.count && flags[i] & Self.degradeFlags != 0 {
                forceFullScan = true
            }
            pendingPaths.insert(path)
        }
        if let newest = eventIds.max() {
            pendingEventCursor = max(pendingEventCursor, UInt64(newest))
        }
        if paths.isEmpty { forceFullScan = true }  // 경로 추출 실패 — 보수적으로 풀스캔
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
        let generation = streamGeneration
        isRecomputing = true
        pendingSince = nil
        let root = watchRoot

        // 이번 배치의 변경 경로를 소비 (재집계 중 도착분은 다음 배치로 — 누락 없음).
        let batch = Array(pendingPaths)
        pendingPaths = []
        let batchCursor = pendingEventCursor
        pendingEventCursor = 0
        let mustFull = forceFullScan
        forceFullScan = false

        // 재베이스 정책: 신뢰 신호가 없어도 주기적으로 풀스캔 (드리프트 정정).
        let rebaseDue =
            incrementalCount >= Self.rebaseEveryIncrements
            || Date().timeIntervalSince(lastFullScanAt) >= Self.rebaseInterval
        let canIncremental =
            !mustFull && !rebaseDue && !batch.isEmpty && batchCursor > 0 && lastHandle != nil

        var handle: ScanHandle?
        var statusLabel = ""
        var completedFullScan = false
        if canIncremental, let prev = lastHandle {
            let report = try? await Task.detached(priority: .background) {
                try prev.rescanPaths(
                    paths: batch, minFileMib: AppModel.scanRecordMinFileMib,
                    dbPath: AppModel.dbPath, fseventCursor: batchCursor)
            }.value
            if let report {
                handle = report.handle
                if report.degraded {
                    incrementalCount = 0
                    lastFullScanAt = Date()
                    statusLabel = "강등 풀스캔 (\(report.degradeReason))"
                } else {
                    incrementalCount += 1
                    statusLabel = "증분 재집계 \(report.rescannedDirs)개 서브트리"
                }
            }
        }
        if handle == nil {
            // 풀스캔 경로 (최초/강등/재베이스/증분 실패).
            // 시작 직전 cursor를 저장하면 스캔 도중 이벤트도 다음 batch에서 재반영된다.
            let checkpoint = UInt64(FSEventsGetCurrentEventId())
            handle = try? await Task.detached(priority: .background) {
                // 스냅샷도 저장해 '변화' 탭에 축적.
                try scanAndSaveWithCursor(
                    path: root, minFileMib: AppModel.scanRecordMinFileMib,
                    dbPath: AppModel.dbPath, fseventCursor: checkpoint)
            }.value
            completedFullScan = handle != nil
            if statusLabel.isEmpty { statusLabel = rebaseDue ? "재베이스 풀스캔" : "풀스캔" }
        }

        // stop/restart 중 완료된 이전 세대 결과는 현재 watcher 상태를 건드리지 않는다.
        guard generation == streamGeneration, root == watchRoot else { return }
        isRecomputing = false
        guard let handle else {
            status = "재집계 실패"
            // DB cursor가 전진하지 않았으므로 현재 session에서도 같은 변경을 다시 시도한다.
            pendingPaths.formUnion(batch)
            pendingEventCursor = max(pendingEventCursor, batchCursor)
            forceFullScan = true
            scheduleRecompute(delay: 60)
            return
        }
        if completedFullScan {
            incrementalCount = 0
            lastFullScanAt = Date()
        }
        lastHandle = handle
        let total = (try? handle.nodeAt(indexPath: []).allocatedSize) ?? 0
        lastTotal = total
        lastUpdated = Date()
        status = "\(statusLabel) · \(humanBytes(total)) · \(shortNow())"

        if budgetBytes > 0 && total > budgetBytes {
            notifyBudget(total: total, budget: budgetBytes, root: root)
        }
        await checkGrowth(root: root)
    }

    /// Growth Watch — 직전 스냅샷 대비 증가가 임계를 넘으면 주범 경로와 함께 알림.
    /// 스냅샷·diff 인프라를 그대로 재사용한다 (recompute가 방금 새 스냅샷을 저장했음).
    private func checkGrowth(root: String) async {
        guard growthAlertBytes > 0 else { return }
        let db = AppModel.dbPath
        let threshold = growthAlertBytes
        let alreadyNotified = lastGrowthNotifiedSnapId

        let result: (newId: Int64, growth: Int64, culprit: String?)? = await Task.detached(
            priority: .utility
        ) {
            guard let snaps = try? listSnapshots(dbPath: db, rootPath: root), snaps.count >= 2
            else { return nil }
            let newest = snaps[0]
            guard newest.scanId != alreadyNotified else { return nil }
            guard let diff = try? openDiff(dbPath: db, oldId: snaps[1].scanId, newId: newest.scanId)
            else { return nil }
            let totals = diff.totals(path: [])
            guard totals.delta > 0, UInt64(totals.delta) >= threshold else { return nil }
            // 주범: |delta| 상위 1개 (임계의 1/10 이상만 의미 있는 귀속으로 간주).
            let minCulpritMib = max(64, threshold / 1_048_576 / 10)
            let culprit = diff.culprits(minDeltaMib: minCulpritMib).first
            let label = culprit.map { "\($0.path) (+\(humanBytes(UInt64(max(0, $0.delta)))))" }
            return (newest.scanId, totals.delta, label)
        }.value

        guard let r = result else { return }
        lastGrowthNotifiedSnapId = r.newId
        let content = UNMutableNotificationContent()
        content.title = "디스크 급증 감지"
        content.body =
            "\(URL(fileURLWithPath: root).lastPathComponent): 직전 스냅샷 대비 +\(humanBytes(UInt64(r.growth)))"
            + (r.culprit.map { " — 주범: \($0)" } ?? "")
        content.sound = .default
        let req = UNNotificationRequest(
            identifier: "growth-\(root)-\(r.newId)", content: content, trigger: nil)
        UNUserNotificationCenter.current().add(req, withCompletionHandler: nil)
        status = "급증 감지 +\(humanBytes(UInt64(r.growth))) · \(shortNow())"
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
