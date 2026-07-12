import Foundation
import SpaceMeshCore

/// 휴지통으로 보낸 항목 하나 — undo에 필요한 (원위치, 휴지통 내 위치) 쌍.
struct TrashRecord {
    let original: String
    let trashURL: URL
    let size: UInt64
}

/// path가 ancestor와 같거나 그 아래인가 — 조상/자손이 함께 선택돼 같은 바이트를
/// 두 번 세는 것을 막는 규칙. 정리 탭 선택과 회수 플랜(F1)이 같은 규칙을 쓴다.
func isSameOrDescendant(_ path: String, of ancestor: String) -> Bool {
    path == ancestor || path.hasPrefix(ancestor + "/")
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

    /// handle이 root를 덮는 스캔 트리를 들고 있고 임계 조건이 맞으면 재스캔 없이
    /// 트리를 재사용한다 (PERF-001). 아니면 기존 전체 재스캔 경로로 폴백.
    func findDups(root: String, minMib: UInt64, handle: ScanHandle?, scannedRoot: String) {
        guard !isFindingDups else { return }
        isFindingDups = true
        dupStartedAt = Date()
        selectedDupPaths = []
        let reusable =
            handle != nil
            && !scannedRoot.isEmpty
            && (root == scannedRoot || root.hasPrefix(scannedRoot + "/"))
            && minMib >= AppModel.scanRecordMinFileMib
        Task {
            do {
                let groups: [DupGroupInfo]
                if reusable, let handle {
                    let subroot = root == scannedRoot ? "" : root
                    groups = await Task.detached(priority: .userInitiated) {
                        handle.findDuplicatesInTree(subroot: subroot, minSizeMib: minMib)
                    }.value
                } else {
                    groups = try await Task.detached(priority: .userInitiated) {
                        try findDuplicates(root: root, minSizeMib: minMib)
                    }.value
                }
                self.dupGroups = groups
            } catch {
                self.message = "중복 검사 실패 — \(humanMessage(for: error))"
            }
            self.dupSearched = true
            self.isFindingDups = false
        }
    }

    /// 정리 후보 선택 — 조상/자손 경로가 이미 선택돼 있으면 해제해 이중 계산을 막는다.
    func toggleCleanupSelection(_ path: String, on: Bool) {
        if on {
            selectedCleanupPaths = selectedCleanupPaths.filter {
                !isSameOrDescendant($0, of: path) && !isSameOrDescendant(path, of: $0)
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
        let (batch, skipped) = moveToTrash(paths: paths)
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
        let restored = restoreFromTrash(lastBatch)
        message = "복원: \(restored)/\(lastBatch.count)개"
        lastBatch = []
    }
}

// MARK: - 휴지통 실행 엔진 (CleanupModel · ReclaimPlan 공용)

/// 안전 가드를 통과한 경로만 휴지통으로 옮긴다. 반환: (이동 기록, 건너뛴 수).
/// 상태를 만지지 않으므로 회수 플랜(F1)도 같은 엔진을 그대로 쓴다.
func moveToTrash(paths: [(path: String, size: UInt64)]) -> (batch: [TrashRecord], skipped: Int) {
    var batch: [TrashRecord] = []
    var skipped = 0
    let fm = FileManager.default
    for item in paths {
        guard CleanupModel.isSafeToTrash(item.path) else {
            skipped += 1
            continue
        }
        var resultURL: NSURL?
        do {
            try fm.trashItem(at: URL(fileURLWithPath: item.path), resultingItemURL: &resultURL)
            if let trashURL = resultURL as URL? {
                batch.append(
                    TrashRecord(original: item.path, trashURL: trashURL, size: item.size))
            }
        } catch {
            skipped += 1
        }
    }
    return (batch, skipped)
}

/// 배치를 원위치로 되돌린다. 반환: 복원 성공 수.
@discardableResult
func restoreFromTrash(_ batch: [TrashRecord]) -> Int {
    let fm = FileManager.default
    var restored = 0
    for record in batch.reversed() {
        do {
            try fm.moveItem(at: record.trashURL, to: URL(fileURLWithPath: record.original))
            restored += 1
        } catch {
            // 이미 휴지통이 비워졌거나 원위치에 새 항목이 생긴 경우.
        }
    }
    return restored
}
