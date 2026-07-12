import CoreServices
import Foundation
import SpaceMeshCore

@MainActor
final class AppModel: ObservableObject {
    @Published var handle: ScanHandle?
    @Published var isScanning = false
    @Published var errorMessage: String?

    /// 현재 위치 (루트 기준 원본 child index 경로).
    @Published var indexPath: [UInt32] = []
    /// indexPath와 나란히 유지되는 디렉토리 이름 (breadcrumb 표시용).
    @Published var breadcrumb: [String] = []

    @Published var currentNode: NodeInfo?
    @Published var currentPath: String = ""
    @Published var children: [NodeInfo] = []
    @Published var bigFiles: [BigFile] = []
    /// 트리 전체의 "크고 오래 방치된" 파일 top-N (읽기 전용 랭킹).
    @Published var staleFiles: [BigFile] = []
    /// 회수 가능 합계 (툴바 상시 노출용, git 조회 없는 경량 계산).
    @Published var reclaimSummary: ReclaimSummary?
    @Published var stats: ScanStatsInfo?
    /// 스캔한 루트가 점유한 allocated 총량 — 디스크 사용량이 아니라 **스캔 범위**의 크기다.
    /// 프로젝트 폴더 하나를 스캔하면 이 값도 그 폴더 크기일 뿐이다.
    @Published var rootAllocated: UInt64 = 0
    @Published var scanSeconds: Double?
    @Published var scanStartedAt = Date()
    /// 마지막으로 스캔한 루트 경로 (카테고리 뷰의 캐시 무효화 기준).
    @Published var scannedRoot: String = ""

    /// 스캔 루트가 올라앉은 볼륨의 실제 용량과 남은 공간.
    ///
    /// 이게 없으면 툴바가 스캔 범위(예: 45 GiB)를 디스크 사용량인 양 보여주게 된다 —
    /// 실제로는 765 GB를 쓰고 있는데. "얼마나 비워야 하나"에 답하려면 분모가 필요하다.
    @Published var volumeTotal: UInt64 = 0
    @Published var volumeFree: UInt64 = 0
    @Published var volumeUsed: UInt64 = 0

    /// 스캔 범위가 디스크 사용량의 몇 퍼센트인지. 볼륨을 못 읽으면 nil.
    var scanCoverage: Double? {
        guard volumeUsed > 0, rootAllocated > 0 else { return nil }
        return min(1.0, Double(rootAllocated) / Double(volumeUsed))
    }

    /// 스캔 루트가 속한 볼륨을 조회한다. 실패하면 0으로 남겨 UI가 표시를 접는다 —
    /// 모르는 값을 지어내느니 안 보여주는 편이 낫다.
    func refreshVolume(path: String) {
        let url = URL(fileURLWithPath: path)
        guard
            let values = try? url.resourceValues(forKeys: [
                .volumeTotalCapacityKey,
                .volumeAvailableCapacityForImportantUsageKey,
            ]),
            let total = values.volumeTotalCapacity,
            let available = values.volumeAvailableCapacityForImportantUsage
        else {
            volumeTotal = 0
            volumeFree = 0
            volumeUsed = 0
            return
        }
        // volumeAvailableCapacityForImportantUsage는 purgeable 공간까지 포함한 "실제로 쓸 수 있는" 값이라
        // Finder가 보여주는 여유 공간과 일치한다.
        volumeTotal = UInt64(max(0, total))
        volumeFree = UInt64(max(0, available))
        volumeUsed = volumeTotal > volumeFree ? volumeTotal - volumeFree : 0
    }

    func startScan(path: String) {
        guard !isScanning else { return }
        isScanning = true
        errorMessage = nil
        scanSeconds = nil
        // 스캔 결과가 나오기 전에 분모부터 잡아둔다 — 볼륨 조회는 즉시 끝난다.
        refreshVolume(path: path)
        let started = Date()
        scanStartedAt = started
        Task {
            // 캐시 로드와 fresh scan을 동시에 시작한다. warm start는 캐시가 먼저 화면을 채운다.
            let freshTask = Task { try await Self.runScan(path: path) }
            if let cached = await Self.loadCached(path: path) {
                self.apply(handle: cached, path: path)
                self.reclaimSummary = nil
            }
            do {
                let handle = try await freshTask.value
                self.apply(handle: handle, path: path)
                self.scanSeconds = Date().timeIntervalSince(started)
                self.reclaimSummary = await Task.detached(priority: .utility) {
                    handle.reclaimSummary()
                }.value
            } catch {
                self.errorMessage = humanMessage(for: error)
            }
            self.isScanning = false
        }
    }

    private func apply(handle: ScanHandle, path: String) {
        self.handle = handle
        self.scannedRoot = path
        self.stats = handle.stats()
        self.indexPath = []
        self.breadcrumb = []
        self.rootAllocated = (try? handle.nodeAt(indexPath: []).allocatedSize) ?? 0
        self.staleFiles = handle.staleFiles(limit: 20, minAgeDays: Self.staleAgeDays)
        self.reload()
    }

    /// 방치 파일로 간주하는 최소 경과일.
    nonisolated static let staleAgeDays: UInt32 = 180

    /// 스캔 시 개별 파일로 기록하는 최소 크기(MiB) — 중복 검사 트리 재사용 조건의 기준.
    /// DuplicatesView 기본값(10MiB)과 맞춰 기본 중복 검색이 재스캔 없이 트리를
    /// 재사용하게 한다. 실측(1.86M files): 50→10 하향 비용은 스캔 시간 동일,
    /// big_files 283→2,348행, DB 크기 동일(14M) — 사실상 0.
    nonisolated static let scanRecordMinFileMib: UInt64 = 10

    /// 스냅샷 DB 경로 (~/Library/Application Support/space-mesh/snapshots.db).
    nonisolated static var dbPath: String {
        let dir = FileManager.default.urls(
            for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("space-mesh")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("snapshots.db").path
    }

    /// 스캔은 블로킹 — detached 태스크에서 실행. 결과는 스냅샷 DB에 자동 저장해
    /// 시계열 diff(변화 탭)가 축적되게 한다. 저장 실패 시 스캔만이라도 수행.
    private nonisolated static func runScan(path: String) async throws -> ScanHandle {
        try await Task.detached(priority: .userInitiated) {
            do {
                // 스캔 도중 발생한 변경은 이 cursor 이후 journal replay로 다시 반영된다.
                let cursor = UInt64(FSEventsGetCurrentEventId())
                return try scanAndSaveWithCursor(
                    path: path, minFileMib: Self.scanRecordMinFileMib,
                    dbPath: Self.dbPath, fseventCursor: cursor)
            } catch {
                return try scanPath(path: path, minFileMib: Self.scanRecordMinFileMib)
            }
        }.value
    }

    private nonisolated static func loadCached(path: String) async -> ScanHandle? {
        await Task.detached(priority: .userInitiated) {
            try? loadSnapshot(dbPath: Self.dbPath, rootPath: path)
        }.value
    }

    func drill(into node: NodeInfo) {
        guard node.childCount > 0 || node.fileCount > 0 else { return }
        indexPath.append(node.index)
        breadcrumb.append(node.name)
        reload()
    }

    /// breadcrumb에서 depth 단계(0 = 루트)로 이동.
    func navigate(toDepth depth: Int) {
        guard depth <= indexPath.count else { return }
        indexPath = Array(indexPath.prefix(depth))
        breadcrumb = Array(breadcrumb.prefix(depth))
        reload()
    }

    func reload() {
        guard let handle else { return }
        do {
            currentNode = try handle.nodeAt(indexPath: indexPath)
            children = try handle.children(indexPath: indexPath)
            bigFiles = try handle.bigFilesAt(indexPath: indexPath)
            currentPath = try handle.fullPath(indexPath: indexPath)
        } catch {
            errorMessage = humanMessage(for: error)
        }
    }

    func fullPath(of node: NodeInfo) -> String? {
        try? handle?.fullPath(indexPath: indexPath + [node.index])
    }

    /// 방치 대용량 목록 재계산 (StaleView 새로고침 — 트리 상주라 즉시).
    func refreshStale() {
        guard let handle else { return }
        staleFiles = handle.staleFiles(limit: 50, minAgeDays: Self.staleAgeDays)
    }

    /// 휴지통 이동 직후 목록에서 제거 (다음 스캔 전까지의 로컬 반영).
    func removeStale(paths: Set<String>) {
        staleFiles.removeAll { paths.contains($0.path) }
    }
}

/// modifiedEpoch(unix 초) → "382일" 경과 라벨. 0(알 수 없음)이면 nil.
func ageDaysLabel(_ modifiedEpoch: Int64) -> String? {
    guard modifiedEpoch > 0 else { return nil }
    let days = Int(max(0, Date().timeIntervalSince1970 - Double(modifiedEpoch)) / 86_400)
    return "\(days)일"
}

func humanBytes(_ bytes: UInt64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"]
    var value = Double(bytes)
    var unit = 0
    while value >= 1024, unit < units.count - 1 {
        value /= 1024
        unit += 1
    }
    return unit == 0 ? "\(bytes) B" : String(format: "%.1f %@", value, units[unit])
}

/// 화면에 띄울 문장. `"\(error)"`로 찍으면 Rust enum 껍데기가 그대로 나온다 —
/// 사용자가 실제로 본 건 `Io(msg: "No such file or directory (os error 2)")` 였다.
/// 경로 오타 하나에 os error 번호를 읽힐 이유가 없다.
func humanMessage(for error: Error) -> String {
    if let scanError = error as? ScanError {
        switch scanError {
        case .Io(let msg): return msg
        case .Snapshot(let msg): return msg
        case .InvalidPath: return "경로가 올바르지 않습니다"
        }
    }
    return error.localizedDescription
}
