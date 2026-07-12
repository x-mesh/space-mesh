import Foundation
import SpaceMeshCore

/// `swift run SpaceMeshApp --selftest` — GUI 없이 FFI 경로를 end-to-end 검증한다.
enum SelfTest {
    /// App.init(MainActor)에서 호출 — ReclaimPlan 등 MainActor 모델을 그대로 검증하려고
    /// 격리를 유지한다.
    @MainActor
    static func runIfRequested() {
        guard CommandLine.arguments.contains("--selftest") else { return }
        do {
            // 픽스처 생성.
            let tmp = FileManager.default.temporaryDirectory
                .appendingPathComponent("space-mesh-selftest-\(ProcessInfo.processInfo.processIdentifier)")
            try? FileManager.default.removeItem(at: tmp)
            try FileManager.default.createDirectory(
                at: tmp.appendingPathComponent("sub"), withIntermediateDirectories: true)
            try Data(repeating: 0xAA, count: 3_000_000)
                .write(to: tmp.appendingPathComponent("big.bin"))
            try Data(repeating: 0xBB, count: 10_000)
                .write(to: tmp.appendingPathComponent("sub/small.bin"))

            let handle = try scanPath(path: tmp.path, minFileMib: 1)
            let stats = handle.stats()
            let root = try handle.nodeAt(indexPath: [])
            let children = try handle.children(indexPath: [])
            let big = try handle.bigFilesAt(indexPath: [])
            let subPath = try handle.fullPath(indexPath: [children[0].index])

            var failures: [String] = []
            if stats.totalFiles != 2 { failures.append("totalFiles=\(stats.totalFiles) != 2") }
            if root.logicalSize != 3_010_000 {
                failures.append("logicalSize=\(root.logicalSize) != 3010000")
            }
            if children.count != 1 || children[0].name != "sub" {
                failures.append("children mismatch: \(children.map(\.name))")
            }
            if big.count != 1 || !big[0].path.hasSuffix("big.bin") {
                failures.append("bigFiles mismatch: \(big.map(\.path))")
            }
            if !subPath.hasSuffix("/sub") { failures.append("fullPath mismatch: \(subPath)") }

            try? FileManager.default.removeItem(at: tmp)
            failures.append(contentsOf: testCleanupDetection())
            failures.append(contentsOf: testDuplicates())
            failures.append(contentsOf: testTrashUndo())
            failures.append(contentsOf: testSnapshotDiff())
            failures.append(contentsOf: testPersistentIncremental())
            failures.append(contentsOf: testCliPath())
            failures.append(contentsOf: testGitRepos())
            failures.append(contentsOf: testPlanMerge())
            failures.append(contentsOf: testCloneMerge())
            if failures.isEmpty {
                print("SELFTEST OK — files=\(stats.totalFiles) dirs=\(stats.totalDirs) root=\(root.allocatedSize)B + cleanup/dedup/trash-undo/plan/clone")
                exit(0)
            } else {
                print("SELFTEST FAIL:\n  " + failures.joined(separator: "\n  "))
                exit(1)
            }
        } catch {
            print("SELFTEST ERROR: \(error)")
            exit(1)
        }
    }

    /// cursor+hardlink registry 저장 → 재로드 → subtree 증분 병합을 FFI 경계까지 검증.
    private static func testPersistentIncremental() -> [String] {
        let fm = FileManager.default
        let pid = ProcessInfo.processInfo.processIdentifier
        let tmp = fm.temporaryDirectory.appendingPathComponent("space-mesh-resume-\(pid)")
        let db = fm.temporaryDirectory.appendingPathComponent("space-mesh-resume-\(pid).db")
        defer {
            try? fm.removeItem(at: tmp)
            try? fm.removeItem(at: db)
            try? fm.removeItem(atPath: db.path + "-wal")
            try? fm.removeItem(atPath: db.path + "-shm")
        }
        do {
            let sub = tmp.appendingPathComponent("sub")
            try fm.createDirectory(at: sub, withIntermediateDirectories: true)
            let original = sub.appendingPathComponent("original.bin")
            try Data(repeating: 7, count: 20_000).write(to: original)
            try fm.linkItem(at: original, to: sub.appendingPathComponent("linked.bin"))

            _ = try scanAndSaveWithCursor(
                path: tmp.path, minFileMib: 1, dbPath: db.path, fseventCursor: 123)
            let restored = try loadSnapshotState(dbPath: db.path, rootPath: tmp.path)
            guard restored.incrementalReady, restored.fseventCursor == 123 else {
                return [
                    "resume: state ready=\(restored.incrementalReady) cursor=\(restored.fseventCursor)"
                ]
            }

            try Data(repeating: 3, count: 30_000)
                .write(to: sub.appendingPathComponent("added.bin"))
            let report = try restored.handle.rescanPaths(
                paths: [sub.path], minFileMib: 1, dbPath: db.path, fseventCursor: 124)
            if report.degraded {
                return ["resume: 증분 병합이 full scan으로 강등됨 — \(report.degradeReason)"]
            }
            let saved = try loadSnapshotState(dbPath: db.path, rootPath: tmp.path)
            if saved.fseventCursor != 124 {
                return ["resume: 갱신 cursor=\(saved.fseventCursor) != 124"]
            }
            if saved.handle.stats().totalFiles != 2 {
                // hardlink 두 경로는 하나로 집계하고 added.bin을 더해 총 2개다.
                return ["resume: totalFiles=\(saved.handle.stats().totalFiles) != 2"]
            }
            return []
        } catch {
            return ["resume: \(error)"]
        }
    }

    /// detectCleanup — 가짜 홈에 Homebrew 캐시 픽스처를 만들어 룰 매칭 확인.
    private static func testCleanupDetection() -> [String] {
        let fm = FileManager.default
        let fakeHome = fm.temporaryDirectory
            .appendingPathComponent("space-mesh-home-\(ProcessInfo.processInfo.processIdentifier)")
        defer { try? fm.removeItem(at: fakeHome) }
        do {
            let brew = fakeHome.appendingPathComponent("Library/Caches/Homebrew")
            try fm.createDirectory(at: brew, withIntermediateDirectories: true)
            try Data(repeating: 1, count: 60_000).write(to: brew.appendingPathComponent("pkg.tar"))
            let found = detectCleanup(home: fakeHome.path)
            guard let hit = found.first(where: { $0.ruleId == "homebrew-cache" }) else {
                return ["cleanup: homebrew-cache 미탐지 (\(found.map(\.ruleId)))"]
            }
            var failures: [String] = []
            if hit.fileCount != 1 { failures.append("cleanup: fileCount=\(hit.fileCount) != 1") }
            if hit.safety != "safe" { failures.append("cleanup: safety=\(hit.safety)") }
            return failures
        } catch {
            return ["cleanup: \(error)"]
        }
    }

    /// findDuplicates — 동일 내용 2벌 + 유일 1개.
    private static func testDuplicates() -> [String] {
        let fm = FileManager.default
        let tmp = fm.temporaryDirectory
            .appendingPathComponent("space-mesh-dup-\(ProcessInfo.processInfo.processIdentifier)")
        defer { try? fm.removeItem(at: tmp) }
        do {
            try fm.createDirectory(at: tmp, withIntermediateDirectories: true)
            let content = Data(repeating: 9, count: 2_000_000)  // 2MB (최소 1MiB 이상)
            try content.write(to: tmp.appendingPathComponent("a.bin"))
            try content.write(to: tmp.appendingPathComponent("b.bin"))
            try Data(repeating: 3, count: 1_500_000).write(to: tmp.appendingPathComponent("c.bin"))
            let groups = try findDuplicates(root: tmp.path, minSizeMib: 1)
            guard groups.count == 1, groups[0].files.count == 2 else {
                return ["dedup: 그룹 결과 이상 — \(groups.map { $0.files.count })"]
            }
            return []
        } catch {
            return ["dedup: \(error)"]
        }
    }

    /// 스냅샷 2회 저장 → diff로 변화 귀속 확인 (FFI 경로).
    private static func testSnapshotDiff() -> [String] {
        let fm = FileManager.default
        let pid = ProcessInfo.processInfo.processIdentifier
        let tmp = fm.temporaryDirectory.appendingPathComponent("space-mesh-diffst-\(pid)")
        let db = fm.temporaryDirectory.appendingPathComponent("space-mesh-diffst-\(pid).db")
        defer {
            try? fm.removeItem(at: tmp)
            try? fm.removeItem(at: db)
        }
        do {
            try fm.createDirectory(
                at: tmp.appendingPathComponent("grow"), withIntermediateDirectories: true)
            try Data(repeating: 1, count: 100_000)
                .write(to: tmp.appendingPathComponent("grow/base.bin"))
            _ = try scanAndSave(path: tmp.path, minFileMib: 1, dbPath: db.path)
            try Data(repeating: 2, count: 3_000_000)
                .write(to: tmp.appendingPathComponent("grow/new.bin"))
            _ = try scanAndSave(path: tmp.path, minFileMib: 1, dbPath: db.path)

            let snaps = try listSnapshots(dbPath: db.path, rootPath: tmp.path)
            guard snaps.count == 2 else { return ["diff: snapshots=\(snaps.count) != 2"] }
            let entries = try diffSnapshots(
                dbPath: db.path, oldId: snaps[1].scanId, newId: snaps[0].scanId, minDeltaMib: 1)
            guard let first = entries.first else { return ["diff: 결과 없음"] }
            var failures: [String] = []
            if !first.path.hasSuffix("grow") { failures.append("diff: path=\(first.path)") }
            if first.delta < 3_000_000 { failures.append("diff: delta=\(first.delta)") }

            // drilldown: 루트 레벨 자식에 grow가 보이고, grow 안에서 직속 파일 잔차가 잡혀야 함.
            let handle = try openDiff(
                dbPath: db.path, oldId: snaps[1].scanId, newId: snaps[0].scanId)
            let rootChildren = handle.children(path: [])
            guard let grow = rootChildren.first(where: { $0.name == "grow" }) else {
                return failures + ["drill: root children에 grow 없음 (\(rootChildren.map(\.name)))"]
            }
            if grow.delta < 3_000_000 { failures.append("drill: grow delta=\(grow.delta)") }
            let inside = handle.children(path: ["grow"])
            // new.bin(3MB)은 기록 임계값(1MiB) 이상 — 실제 파일 이름으로 잡혀야 한다.
            if !inside.contains(where: {
                $0.kind == "file" && $0.name == "new.bin" && $0.delta >= 3_000_000 && $0.before == 0
            }) {
                failures.append(
                    "drill: new.bin 파일 행 미검출 (\(inside.map { "\($0.kind):\($0.name)" }))")
            }
            return failures
        } catch {
            return ["diff: \(error)"]
        }
    }

    /// git_repos() FFI — 임시 fixture repo(dirty)를 만들어 스캔 트리에서 감지·분류하는지.
    private static func testGitRepos() -> [String] {
        let fm = FileManager.default
        let base = fm.temporaryDirectory
            .appendingPathComponent("space-mesh-gitst-\(ProcessInfo.processInfo.processIdentifier)")
        defer { try? fm.removeItem(at: base) }
        func run(_ args: [String], _ cwd: URL) -> Bool {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: "/usr/bin/env")
            p.arguments = ["git", "-C", cwd.path] + args
            p.environment = ["GIT_TERMINAL_PROMPT": "0"]
            p.standardOutput = FileHandle.nullDevice
            p.standardError = FileHandle.nullDevice
            try? p.run()
            p.waitUntilExit()
            return p.terminationStatus == 0
        }
        do {
            let repo = base.appendingPathComponent("myrepo")
            try fm.createDirectory(at: repo, withIntermediateDirectories: true)
            guard run(["init", "-q", "-b", "main"], repo) else {
                return []  // git 미설치 환경이면 스킵 (crash 없음이 목표)
            }
            _ = run(["config", "user.email", "t@t.t"], repo)
            _ = run(["config", "user.name", "t"], repo)
            try Data("hello".utf8).write(to: repo.appendingPathComponent("a.txt"))
            _ = run(["add", "-A"], repo)
            _ = run(["commit", "-q", "-m", "init"], repo)
            // 미커밋 변경 → danger 기대.
            try Data("changed".utf8).write(to: repo.appendingPathComponent("a.txt"))

            let handle = try scanPath(path: base.path, minFileMib: 50)
            let report = handle.gitRepos(includeSubmodules: false)
            guard let hit = report.repos.first(where: { $0.path.hasSuffix("myrepo") }) else {
                return ["git: fixture repo 미감지 (\(report.repos.map(\.path)))"]
            }
            var failures: [String] = []
            if hit.risk != "danger" { failures.append("git: dirty repo risk=\(hit.risk) != danger") }
            if hit.trackedDirty != 1 { failures.append("git: trackedDirty=\(hit.trackedDirty) != 1") }
            if hit.head != "branch:main" { failures.append("git: head=\(hit.head)") }
            return failures
        } catch {
            return ["git: \(error)"]
        }
    }

    /// 주기 모드가 등록할 CLI 바이너리를 실제로 찾을 수 있는지 (--diff 실행 가능한지).
    private static func testCliPath() -> [String] {
        guard let cli = BackgroundAgent.cliPath() else {
            return ["cli: 바이너리 경로 탐색 실패 (개발 빌드면 core/target/release/space-mesh 필요)"]
        }
        // 실제로 --version이 도는지 확인 (launchd가 실행할 바로 그 바이너리).
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: cli)
        proc.arguments = ["--version"]
        proc.standardOutput = FileHandle.nullDevice
        proc.standardError = FileHandle.nullDevice
        do {
            try proc.run()
            proc.waitUntilExit()
            return proc.terminationStatus == 0 ? [] : ["cli: --version 종료코드 \(proc.terminationStatus)"]
        } catch {
            return ["cli: 실행 실패 \(error)"]
        }
    }

    /// 휴지통 이동 + 복원 + 안전 가드.
    private static func testTrashUndo() -> [String] {
        var failures: [String] = []
        // 가드: 홈 밖 / 홈 직속 최상위는 거부.
        if CleanupModel.isSafeToTrash("/tmp/x") { failures.append("guard: /tmp/x 허용됨") }
        if CleanupModel.isSafeToTrash(NSHomeDirectory() + "/Library") {
            failures.append("guard: ~/Library 통째 삭제 허용됨")
        }
        if !CleanupModel.isSafeToTrash(NSHomeDirectory() + "/Library/Caches/Foo") {
            failures.append("guard: ~/Library/Caches/Foo 거부됨")
        }

        // 실제 trash + 복원 (홈 아래 픽스처).
        let fm = FileManager.default
        let fixture = URL(fileURLWithPath: NSHomeDirectory())
            .appendingPathComponent(".space-mesh-selftest/sub")
        defer { try? fm.removeItem(at: fixture.deletingLastPathComponent()) }
        do {
            try fm.createDirectory(at: fixture, withIntermediateDirectories: true)
            let file = fixture.appendingPathComponent("victim.bin")
            try Data(repeating: 5, count: 10_000).write(to: file)
            var trashURL: NSURL?
            try fm.trashItem(at: file, resultingItemURL: &trashURL)
            if fm.fileExists(atPath: file.path) {
                failures.append("trash: 원본이 남아 있음")
            }
            guard let restored = trashURL as URL? else {
                return failures + ["trash: resultingItemURL 없음"]
            }
            try fm.moveItem(at: restored, to: file)
            if !fm.fileExists(atPath: file.path) {
                failures.append("undo: 복원 실패")
            }
        } catch {
            failures.append("trash/undo: \(error)")
        }
        return failures
    }

    /// 회수 플랜 병합 규칙 (F1) — 조상/자손이 함께 담겨 같은 바이트를 두 번 세면 안 된다.
    @MainActor
    private static func testPlanMerge() -> [String] {
        let plan = ReclaimPlan()
        let child = PlanItem(
            path: "/Users/x/proj/node_modules/sub",
            deletePaths: ["/Users/x/proj/node_modules/sub"], estimatedBytes: 100,
            source: .category, safety: "safe", recreateCommand: "")
        let parent = PlanItem(
            path: "/Users/x/proj/node_modules", deletePaths: ["/Users/x/proj/node_modules"],
            estimatedBytes: 500, source: .category, safety: "safe", recreateCommand: "")
        var failures: [String] = []

        // 자손 → 조상: 조상이 자손을 밀어낸다.
        plan.add(child)
        plan.add(parent)
        if plan.items.map(\.path) != [parent.path] {
            failures.append("plan: 조상 추가 시 자손 미제거 (\(plan.items.map(\.path)))")
        }
        // 조상이 있으면 자손은 무시.
        plan.add(child)
        if plan.items.count != 1 {
            failures.append("plan: 조상 존재 시 자손이 추가됨")
        }
        // 중복 추가 무시 + 합계.
        plan.add(parent)
        if plan.items.count != 1 || plan.totalEstimated != 500 {
            failures.append(
                "plan: 중복/합계 오류 (count=\(plan.items.count), total=\(plan.totalEstimated))")
        }
        return failures
    }

    /// mergeDuplicates (F3) — 내용이 다르면 거부, 같으면 병합 후에도 두 파일 내용이 보존된다.
    /// 삭제가 아니라 블록 공유이므로 victim 파일은 사라지지 않아야 한다.
    private static func testCloneMerge() -> [String] {
        let fm = FileManager.default
        let tmp = fm.temporaryDirectory
            .appendingPathComponent("space-mesh-selftest-clone-\(ProcessInfo.processInfo.processIdentifier)")
        defer { try? fm.removeItem(at: tmp) }
        var failures: [String] = []
        do {
            try fm.createDirectory(at: tmp, withIntermediateDirectories: true)
            let same = Data(repeating: 7, count: 100_000)
            let other = Data(repeating: 8, count: 100_000)
            let keep = tmp.appendingPathComponent("keep.bin")
            let victim = tmp.appendingPathComponent("victim.bin")
            let differ = tmp.appendingPathComponent("differ.bin")
            try same.write(to: keep)
            try same.write(to: victim)
            try other.write(to: differ)

            // 내용이 다르면 반드시 거부하고 원본을 건드리지 않는다.
            let bad = mergeDuplicates(keep: keep.path, victims: [differ.path])
            if bad.merged != 0 || bad.failed != 1 {
                failures.append("clone: 내용이 다른데 병합됨 (merged=\(bad.merged))")
            }
            if try Data(contentsOf: differ) != other {
                failures.append("clone: 거부 후 원본이 훼손됨")
            }

            // 동일 파일 병합 — APFS면 성공하고, 어느 쪽이든 두 파일 다 남아야 한다.
            let ok = mergeDuplicates(keep: keep.path, victims: [victim.path])
            if ok.merged + ok.failed != 1 {
                failures.append("clone: 결과 집계 오류 (merged=\(ok.merged) failed=\(ok.failed))")
            }
            if try Data(contentsOf: victim) != same || Data(contentsOf: keep) != same {
                failures.append("clone: 병합 후 내용이 바뀜 — 무손실이어야 한다")
            }
        } catch {
            failures.append("clone: \(error)")
        }
        return failures
    }
}
