import Foundation
import SpaceMeshCore

/// 휴지통으로 보낸 항목 하나 — undo에 필요한 (원위치, 휴지통 내 위치) 쌍.
struct TrashRecord {
    let original: String
    let trashURL: URL
    let size: UInt64
}

@MainActor
final class CleanupModel: ObservableObject {
    // 룰 기반 정리 후보
    @Published var candidates: [CleanupCandidate] = []
    @Published var isDetecting = false
    @Published var detectStartedAt = Date()
    @Published var selectedCleanupPaths: Set<String> = []

    // 중복 파일
    @Published var dupGroups: [DupGroupInfo] = []
    @Published var isFindingDups = false
    @Published var dupStartedAt = Date()
    @Published var selectedDupPaths: Set<String> = []
    @Published var dupSearched = false
    @Published var isMerging = false

    // 공식 정리 커맨드 제안
    @Published var advices: [ToolAdviceInfo] = []
    @Published var isAdvising = false

    // 삭제/undo
    @Published var lastBatch: [TrashRecord] = []
    @Published var message: String?

    /// 설치된 도구의 공식 cleanup 커맨드 + dry-run 예상 회수량 조회.
    func loadAdvice() {
        guard !isAdvising else { return }
        isAdvising = true
        Task {
            let found = await Task.detached(priority: .utility) { toolAdvice() }.value
            self.advices = found
            self.isAdvising = false
        }
    }

    // MARK: - 탐지

    func detect() {
        guard !isDetecting else { return }
        isDetecting = true
        detectStartedAt = Date()
        selectedCleanupPaths = []
        Task {
            let found = await Task.detached(priority: .userInitiated) {
                detectCleanup(home: NSHomeDirectory())
            }.value
            self.candidates = found
            self.isDetecting = false
        }
    }

    func findDups(root: String, minMib: UInt64) {
        guard !isFindingDups else { return }
        isFindingDups = true
        dupStartedAt = Date()
        selectedDupPaths = []
        Task {
            do {
                let groups = try await Task.detached(priority: .userInitiated) {
                    try findDuplicates(root: root, minSizeMib: minMib)
                }.value
                self.dupGroups = groups
            } catch {
                self.message = "중복 검사 실패: \(error)"
            }
            self.dupSearched = true
            self.isFindingDups = false
        }
    }

    /// 그룹의 나머지를 첫 파일의 APFS 클론으로 교체 — 데이터 손실 없는 회수 (F3).
    /// 각 파일은 교체 직전 재해시로 동일성이 재확인되므로 안전하다.
    func mergeGroupAsClones(_ group: DupGroupInfo, onDone: @escaping @MainActor () -> Void) {
        guard !isMerging, group.files.count > 1 else { return }
        isMerging = true
        let keep = group.files[0]
        let victims = Array(group.files.dropFirst())
        Task {
            let result = await Task.detached(priority: .userInitiated) {
                mergeDuplicates(keep: keep, victims: victims)
            }.value
            self.message =
                "클론 병합 \(result.merged)개 · 회수 \(humanBytes(result.reclaimed))"
                + (result.failed > 0 ? " · 실패 \(result.failed) (비-APFS/변경됨)" : "")
            self.isMerging = false
            onDone()
        }
    }

    /// 정리 후보 선택 — 조상/자손 경로가 이미 선택돼 있으면 해제해 이중 계산을 막는다.
    func toggleCleanupSelection(_ path: String, on: Bool) {
        if on {
            selectedCleanupPaths = selectedCleanupPaths.filter {
                !$0.hasPrefix(path + "/") && !path.hasPrefix($0 + "/")
            }
            selectedCleanupPaths.insert(path)
        } else {
            selectedCleanupPaths.remove(path)
        }
    }

    // MARK: - 안전 가드

    /// 휴지통 이동을 허용하는 경로인지 — 홈 아래이면서, 홈 직속 최상위 디렉토리
    /// 통째(~/Library, ~/Documents 등)는 금지. 시스템 경로는 홈 밖이므로 자동 차단.
    nonisolated static func isSafeToTrash(_ path: String) -> Bool {
        let home = NSHomeDirectory()
        guard path.hasPrefix(home + "/") else { return false }
        let relative = path.dropFirst(home.count + 1)
        return relative.split(separator: "/").count >= 2
    }

    // MARK: - 휴지통 + undo

    /// 선택 경로를 휴지통으로 이동. 반환: (성공 수, 건너뜀 수).
    @discardableResult
    func trash(paths: [(path: String, size: UInt64)]) -> (moved: Int, skipped: Int) {
        var batch: [TrashRecord] = []
        var skipped = 0
        let fm = FileManager.default
        for item in paths {
            guard Self.isSafeToTrash(item.path) else {
                skipped += 1
                continue
            }
            var resultURL: NSURL?
            do {
                try fm.trashItem(
                    at: URL(fileURLWithPath: item.path), resultingItemURL: &resultURL)
                if let trashURL = resultURL as URL? {
                    batch.append(
                        TrashRecord(original: item.path, trashURL: trashURL, size: item.size))
                }
            } catch {
                skipped += 1
            }
        }
        lastBatch = batch
        let freed = batch.reduce(UInt64(0)) { $0 + $1.size }
        if batch.isEmpty {
            message = "이동한 항목이 없습니다" + (skipped > 0 ? " (\(skipped)개 보호됨/실패)" : "")
        } else {
            message = "휴지통으로 \(batch.count)개 이동 · \(humanBytes(freed)) 회수"
                + (skipped > 0 ? " · \(skipped)개 건너뜀" : "") + " — 되돌리기 가능"
        }
        return (batch.count, skipped)
    }

    /// 마지막 배치를 휴지통에서 원위치로 복원.
    func undoLastBatch() {
        let fm = FileManager.default
        var restored = 0
        for record in lastBatch.reversed() {
            do {
                try fm.moveItem(
                    at: record.trashURL, to: URL(fileURLWithPath: record.original))
                restored += 1
            } catch {
                // 이미 휴지통이 비워졌거나 원위치에 새 항목이 생긴 경우.
            }
        }
        message = "복원: \(restored)/\(lastBatch.count)개"
        lastBatch = []
    }
}
