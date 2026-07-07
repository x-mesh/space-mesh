import Foundation
import SpaceMeshCore

/// `swift run SpaceMeshApp --selftest` — GUI 없이 FFI 경로를 end-to-end 검증한다.
enum SelfTest {
    /// App.init(MainActor)에서 호출 — ReclaimPlan 등 MainActor 모델 검증을 위해 격리 유지.
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
            // F5: 나이 축 — 방금 만든 픽스처는 mtime이 현재에 가깝다.
            if root.newestMtime <= 0 { failures.append("newestMtime=\(root.newestMtime) <= 0") }
            if big[0].mtime <= 0 { failures.append("bigFile mtime=\(big[0].mtime) <= 0") }

            try? FileManager.default.removeItem(at: tmp)
            failures.append(contentsOf: testCleanupDetection())
            failures.append(contentsOf: testDuplicates())
            failures.append(contentsOf: testTrashUndo())
            failures.append(contentsOf: testSnapshotDiff())
            failures.append(contentsOf: testCliPath())
            failures.append(contentsOf: testGitRepos())
            failures.append(contentsOf: testRefreshPaths())
            failures.append(contentsOf: testReclaimLog())
            failures.append(contentsOf: testPlanMerge())
            failures.append(contentsOf: testCloneMerge())
            failures.append(contentsOf: testSuggestSchema())
            if failures.isEmpty {
                print("SELFTEST OK — files=\(stats.totalFiles) dirs=\(stats.totalDirs) root=\(root.allocatedSize)B + cleanup/dedup/trash-undo")
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

    /// refreshPaths — 증분 재스캔이 추가/삭제를 반영하고 루트 밖 경로를 무시하는지 (F2).
    private static func testRefreshPaths() -> [String] {
        let fm = FileManager.default
        let tmp = fm.temporaryDirectory
            .appendingPathComponent("space-mesh-refresh-\(ProcessInfo.processInfo.processIdentifier)")
        defer { try? fm.removeItem(at: tmp) }
        do {
            let sub = tmp.appendingPathComponent("sub")
            try fm.createDirectory(at: sub, withIntermediateDirectories: true)
            try Data(repeating: 1, count: 2_000_000).write(to: sub.appendingPathComponent("a.bin"))

            let handle = try scanPath(path: tmp.path, minFileMib: 1)
            let before = try handle.nodeAt(indexPath: []).logicalSize

            // 추가 반영.
            try Data(repeating: 2, count: 5_000_000).write(to: sub.appendingPathComponent("b.bin"))
            let summary = try handle.refreshPaths(absPaths: [sub.path], minFileMib: 1)
            var failures: [String] = []
            if summary.refreshedSubtrees != 1 {
                failures.append("refresh: subtrees=\(summary.refreshedSubtrees) != 1")
            }
            let grown = try handle.nodeAt(indexPath: []).logicalSize
            if grown != before + 5_000_000 {
                failures.append("refresh: 추가 미반영 \(before) → \(grown)")
            }

            // 삭제 반영.
            try fm.removeItem(at: sub.appendingPathComponent("b.bin"))
            _ = try handle.refreshPaths(absPaths: [sub.path], minFileMib: 1)
            let shrunk = try handle.nodeAt(indexPath: []).logicalSize
            if shrunk != before { failures.append("refresh: 삭제 미반영 \(shrunk) != \(before)") }

            // 루트 밖 경로는 무시.
            let outside = try handle.refreshPaths(absPaths: ["/no-such-outside-root"], minFileMib: 1)
            if outside.refreshedSubtrees != 0 {
                failures.append("refresh: 루트 밖 경로가 재스캔됨")
            }

            // 갱신된 트리를 스냅샷으로 저장할 수 있어야 한다 (live 모드 경로).
            let db = tmp.appendingPathComponent("refresh.db")
            if (try? handle.saveToDb(dbPath: db.path)) == nil {
                failures.append("refresh: saveToDb 실패")
            }
            return failures
        } catch {
            return ["refresh: \(error)"]
        }
    }

    /// reclaim_log — 기록/실측/undo 라운드트립 (F1).
    private static func testReclaimLog() -> [String] {
        let fm = FileManager.default
        let db = fm.temporaryDirectory
            .appendingPathComponent("space-mesh-rl-\(ProcessInfo.processInfo.processIdentifier).db")
        defer { try? fm.removeItem(at: db) }
        do {
            let root = "/tmp/reclaim-fixture"
            let id = try reclaimLogAdd(
                dbPath: db.path, rootPath: root, itemCount: 3, estimated: 1_000_000)
            try reclaimLogSetMeasured(dbPath: db.path, id: id, measured: 900_000)
            var records = try reclaimLogList(dbPath: db.path, rootPath: root, limit: 10)
            var failures: [String] = []
            guard records.count == 1 else { return ["reclaim: records=\(records.count) != 1"] }
            if records[0].estimated != 1_000_000 {
                failures.append("reclaim: estimated=\(records[0].estimated)")
            }
            if records[0].measured != 900_000 {
                failures.append("reclaim: measured=\(String(describing: records[0].measured))")
            }
            if records[0].undone { failures.append("reclaim: undone이 초기부터 true") }
            try reclaimLogSetUndone(dbPath: db.path, id: id)
            records = try reclaimLogList(dbPath: db.path, rootPath: root, limit: 10)
            if !records[0].undone { failures.append("reclaim: undone 미반영") }
            return failures
        } catch {
            return ["reclaim: \(error)"]
        }
    }

    /// 회수 플랜의 조상/자손 병합 규칙 — 이중 계산 방지 (F1).
    @MainActor
    private static func testPlanMerge() -> [String] {
        let plan = ReclaimPlan()
        let child = PlanItem(
            path: "/Users/x/proj/node_modules/sub", estimatedBytes: 100,
            source: .category, safety: "safe", recreateCommand: "")
        let parent = PlanItem(
            path: "/Users/x/proj/node_modules", estimatedBytes: 500,
            source: .category, safety: "safe", recreateCommand: "")
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
            failures.append("plan: 중복/합계 오류 (count=\(plan.items.count), total=\(plan.totalEstimated))")
        }
        return failures
    }

    /// mergeDuplicates — 내용 다른 파일은 거부, 동일 파일은 병합 후 내용 보존 (F3).
    private static func testCloneMerge() -> [String] {
        let fm = FileManager.default
        let tmp = fm.temporaryDirectory
            .appendingPathComponent("space-mesh-clone-\(ProcessInfo.processInfo.processIdentifier)")
        defer { try? fm.removeItem(at: tmp) }
        do {
            try fm.createDirectory(at: tmp, withIntermediateDirectories: true)
            let content = Data(repeating: 7, count: 100_000)
            try content.write(to: tmp.appendingPathComponent("keep.bin"))
            try content.write(to: tmp.appendingPathComponent("victim.bin"))
            try Data(repeating: 9, count: 100_000).write(to: tmp.appendingPathComponent("other.bin"))

            var failures: [String] = []
            // 내용이 다른 파일은 어떤 경우에도 병합 거부 — 데이터 손실 방지의 핵심.
            let bad = mergeDuplicates(
                keep: tmp.appendingPathComponent("keep.bin").path,
                victims: [tmp.appendingPathComponent("other.bin").path])
            if bad.merged != 0 { failures.append("clone: 다른 내용이 병합됨") }
            if try Data(contentsOf: tmp.appendingPathComponent("other.bin"))
                != Data(repeating: 9, count: 100_000)
            {
                failures.append("clone: 거부 후 원본 손상")
            }

            // 동일 파일 병합 — APFS(macOS 기본)면 성공하고 내용이 보존돼야 한다.
            let good = mergeDuplicates(
                keep: tmp.appendingPathComponent("keep.bin").path,
                victims: [tmp.appendingPathComponent("victim.bin").path])
            if good.merged == 1 {
                if try Data(contentsOf: tmp.appendingPathComponent("victim.bin")) != content {
                    failures.append("clone: 병합 후 내용 불일치")
                }
            }
            // 비-APFS 임시 볼륨이면 failed=1도 허용 (victim 무손상만 확인).
            if good.merged == 0,
                try Data(contentsOf: tmp.appendingPathComponent("victim.bin")) != content
            {
                failures.append("clone: 병합 실패 시 victim 손상")
            }
            return failures
        } catch {
            return ["clone: \(error)"]
        }
    }

    /// CLI --suggest 산출물을 앱의 Suggestion Codable로 디코드 — 스키마 계약 검증 (F6/F8).
    private static func testSuggestSchema() -> [String] {
        guard let cli = BackgroundAgent.cliPath() else { return [] }  // testCliPath가 이미 보고
        let fm = FileManager.default
        let pid = ProcessInfo.processInfo.processIdentifier
        let tmp = fm.temporaryDirectory.appendingPathComponent("space-mesh-suggest-\(pid)")
        let out = fm.temporaryDirectory.appendingPathComponent("space-mesh-suggest-\(pid).json")
        defer {
            try? fm.removeItem(at: tmp)
            try? fm.removeItem(at: out)
        }
        do {
            try fm.createDirectory(at: tmp, withIntermediateDirectories: true)
            try Data(repeating: 1, count: 10_000).write(to: tmp.appendingPathComponent("f.bin"))
            let proc = Process()
            proc.executableURL = URL(fileURLWithPath: cli)
            proc.arguments = [
                tmp.path, "--suggest", "--suggest-out", out.path,
                "--idle-days", "0", "--suggest-min-mib", "0",
            ]
            proc.standardOutput = FileHandle.nullDevice
            proc.standardError = FileHandle.nullDevice
            try proc.run()
            proc.waitUntilExit()
            guard proc.terminationStatus == 0 else {
                return ["suggest: CLI 종료코드 \(proc.terminationStatus)"]
            }
            let data = try Data(contentsOf: out)
            let decoded = try JSONDecoder().decode(Suggestion.self, from: data)
            if decoded.version != 1 { return ["suggest: version=\(decoded.version)"] }
            if decoded.generatedAt == 0 { return ["suggest: generated_at=0"] }
            return []
        } catch {
            return ["suggest: \(error)"]
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
}
