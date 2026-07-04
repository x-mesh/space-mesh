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
    @Published var stats: ScanStatsInfo?
    @Published var scanSeconds: Double?
    @Published var scanStartedAt = Date()
    /// 마지막으로 스캔한 루트 경로 (카테고리 뷰의 캐시 무효화 기준).
    @Published var scannedRoot: String = ""

    func startScan(path: String) {
        guard !isScanning else { return }
        isScanning = true
        errorMessage = nil
        let started = Date()
        scanStartedAt = started
        Task {
            do {
                let handle = try await Self.runScan(path: path)
                self.handle = handle
                self.scannedRoot = path
                self.stats = handle.stats()
                self.scanSeconds = Date().timeIntervalSince(started)
                self.indexPath = []
                self.breadcrumb = []
                self.staleFiles = handle.staleFiles(limit: 20, minAgeDays: Self.staleAgeDays)
                self.reload()
            } catch {
                self.errorMessage = "\(error)"
            }
            self.isScanning = false
        }
    }

    /// 방치 파일로 간주하는 최소 경과일.
    nonisolated static let staleAgeDays: UInt32 = 180

    /// 스캔 시 개별 파일로 기록하는 최소 크기(MiB) — 중복 검사 트리 재사용 조건의 기준.
    nonisolated static let scanRecordMinFileMib: UInt64 = 50

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
                return try scanAndSave(
                    path: path, minFileMib: Self.scanRecordMinFileMib, dbPath: Self.dbPath)
            } catch {
                return try scanPath(path: path, minFileMib: Self.scanRecordMinFileMib)
            }
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
            errorMessage = "\(error)"
        }
    }

    func fullPath(of node: NodeInfo) -> String? {
        try? handle?.fullPath(indexPath: indexPath + [node.index])
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
