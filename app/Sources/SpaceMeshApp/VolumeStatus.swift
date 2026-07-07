import Foundation

/// 스캔 대상 볼륨의 공간 상태 (F4).
/// free와 freeImportant의 차이가 purgeable — 시스템이 "필요하면 비울 수 있다"고
/// 보류 중인 공간(로컬 스냅샷·휴지통·캐시)이라, 파일을 지워도 여유가 바로 늘지
/// 않는 원인을 설명한다.
struct VolumeInfo {
    let total: UInt64
    /// 지금 즉시 쓸 수 있는 여유.
    let free: UInt64
    /// purgeable을 회수했을 때의 여유 (importantUsage 기준).
    let freeImportant: UInt64
    /// Time Machine 로컬 스냅샷 수.
    let localSnapshots: Int

    var purgeable: UInt64 { freeImportant > free ? freeImportant - free : 0 }
}

@MainActor
final class VolumeStatus: ObservableObject {
    @Published var info: VolumeInfo?

    func refresh(path: String) {
        Task {
            let loaded = await Task.detached(priority: .utility) { Self.load(path: path) }.value
            self.info = loaded
        }
    }

    nonisolated static func load(path: String) -> VolumeInfo? {
        let url = URL(fileURLWithPath: path)
        guard
            let values = try? url.resourceValues(forKeys: [
                .volumeTotalCapacityKey,
                .volumeAvailableCapacityKey,
                .volumeAvailableCapacityForImportantUsageKey,
            ]),
            let total = values.volumeTotalCapacity,
            let free = values.volumeAvailableCapacity
        else { return nil }
        let important = values.volumeAvailableCapacityForImportantUsage ?? Int64(free)
        return VolumeInfo(
            total: UInt64(max(0, total)),
            free: UInt64(max(0, free)),
            freeImportant: UInt64(max(0, important)),
            localSnapshots: countLocalSnapshots()
        )
    }

    /// tmutil listlocalsnapshots / 의 스냅샷 줄 수. tmutil 부재/실패 시 0.
    nonisolated private static func countLocalSnapshots() -> Int {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/usr/bin/tmutil")
        proc.arguments = ["listlocalsnapshots", "/"]
        let pipe = Pipe()
        proc.standardOutput = pipe
        proc.standardError = FileHandle.nullDevice
        do {
            try proc.run()
        } catch {
            return 0
        }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        proc.waitUntilExit()
        guard proc.terminationStatus == 0, let out = String(data: data, encoding: .utf8) else {
            return 0
        }
        return out.split(separator: "\n")
            .filter { $0.contains("com.apple.TimeMachine") }
            .count
    }
}
