use super::*;
use std::fs;
use std::process::Command;

/// 격리된 임시 repo 픽스처. 전역 git config 오염을 피하려 로컬 config만 설정.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "space-git-{}-{}-{}",
            tag,
            std::process::id(),
            now_secs()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Fixture { dir }
    }

    fn git(&self, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git")
            .status
            .success();
        assert!(ok, "git {:?} failed", args);
    }

    fn init(&self) {
        self.git(&["init", "-q", "-b", "main"]);
        self.git(&["config", "user.email", "t@t.t"]);
        self.git(&["config", "user.name", "t"]);
        self.git(&["config", "commit.gpgsign", "false"]);
    }

    fn write(&self, name: &str, content: &str) {
        fs::write(self.dir.join(name), content).unwrap();
    }

    fn commit(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn probe_ok(dir: &Path) -> RepoHealth {
    probe(dir).unwrap_or_else(|e| panic!("probe failed: {:?}", e))
}

#[test]
fn clean_pushed_recent_is_active() {
    let f = Fixture::new("active");
    f.init();
    f.write("a.txt", "hello");
    f.commit("init");
    // 가짜 remote-tracking을 만들어 pushed 상태 흉내: bare remote + push.
    let remote = f.dir.join("../remote.git");
    Command::new("git")
        .args(["init", "--bare", "-q", remote.to_str().unwrap()])
        .output()
        .unwrap();
    f.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    f.git(&["push", "-q", "-u", "origin", "main"]);

    let h = probe_ok(&f.dir);
    assert_eq!(h.upstream, UpstreamState::UpToDate, "{:?}", h);
    assert_eq!(h.tracked_dirty, 0);
    assert!(h.has_remote);
    assert_eq!(h.risk, Risk::Active, "{:?}", h);
    let _ = fs::remove_dir_all(remote);
}

#[test]
fn ahead_of_upstream_is_danger() {
    let f = Fixture::new("ahead");
    f.init();
    f.write("a.txt", "1");
    f.commit("c1");
    let remote = f.dir.join("../ahead-remote.git");
    Command::new("git")
        .args(["init", "--bare", "-q", remote.to_str().unwrap()])
        .output()
        .unwrap();
    f.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    f.git(&["push", "-q", "-u", "origin", "main"]);
    // upstream 대비 2 커밋 ahead (push 안 함).
    f.write("b.txt", "2");
    f.commit("c2");
    f.write("c.txt", "3");
    f.commit("c3");

    let h = probe_ok(&f.dir);
    assert_eq!(h.upstream, UpstreamState::Ahead(2), "{:?}", h);
    assert_eq!(h.risk, Risk::Danger, "{:?}", h);
    let _ = fs::remove_dir_all(remote);
}

#[test]
fn local_only_branch_without_upstream_is_not_danger() {
    // upstream 미설정 로컬 브랜치는 ahead가 커도 NoUpstream (위험 아님).
    let f = Fixture::new("noupstream");
    f.init();
    f.write("a.txt", "1");
    f.commit("c1");
    f.write("b.txt", "2");
    f.commit("c2");
    // remote 없음, upstream 없음.

    let h = probe_ok(&f.dir);
    assert_eq!(h.upstream, UpstreamState::NoUpstream, "{:?}", h);
    // remote 없음 → Caution (백업 경로 없음), Danger 아님.
    assert_eq!(h.risk, Risk::Caution, "{:?}", h);
}

#[test]
fn dirty_recent_is_danger_not_active() {
    // 패널 핵심: 최근 커밋 + dirty가 활성으로 은폐되면 안 됨.
    let f = Fixture::new("dirty");
    f.init();
    f.write("a.txt", "1");
    f.commit("c1");
    f.write("a.txt", "modified"); // tracked dirty
    f.write("new.txt", "untracked"); // untracked

    let h = probe_ok(&f.dir);
    assert_eq!(h.tracked_dirty, 1, "{:?}", h);
    assert!(h.untracked_present);
    assert_eq!(h.risk, Risk::Danger, "{:?}", h);
}

#[test]
fn stash_only_is_danger_and_not_abandoned() {
    // clean 작업트리 + stash만 있는 오래된 repo가 방치로 오분류되면 데이터 유실.
    let f = Fixture::new("stash");
    f.init();
    f.write("a.txt", "1");
    f.commit("c1");
    f.write("a.txt", "wip change");
    f.git(&["stash", "-q"]);
    // 이제 작업트리 clean, stash 1개.

    let h = probe_ok(&f.dir);
    assert_eq!(h.tracked_dirty, 0, "작업트리는 clean이어야: {:?}", h);
    assert_eq!(h.stash_count, 1, "{:?}", h);
    assert_eq!(h.risk, Risk::Danger, "stash-only는 위험: {:?}", h);
}

#[test]
fn unborn_repo_is_caution_not_crash() {
    // git init 직후 커밋 0개.
    let f = Fixture::new("unborn");
    f.init();

    let h = probe_ok(&f.dir);
    assert_eq!(h.head, HeadState::Unborn, "{:?}", h);
    assert_eq!(h.last_commit_days, None);
    assert_eq!(h.risk, Risk::Caution, "{:?}", h);
}

#[test]
fn detached_head_with_commit_is_danger() {
    let f = Fixture::new("detached");
    f.init();
    f.write("a.txt", "1");
    f.commit("c1");
    f.write("b.txt", "2");
    f.commit("c2");
    // 첫 커밋으로 detach.
    let first = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&f.dir)
            .args(["rev-list", "--max-parents=0", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    f.git(&["checkout", "-q", first.trim()]);

    let h = probe_ok(&f.dir);
    assert_eq!(h.head, HeadState::Detached, "{:?}", h);
    assert_eq!(h.risk, Risk::Danger, "{:?}", h);
}

#[test]
fn probe_errors_on_non_repo() {
    let dir = std::env::temp_dir().join(format!("space-git-nonrepo-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let err = probe(&dir).unwrap_err();
    assert_eq!(err.1, ProbeError::NotARepo, "{:?}", err);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn filter_excludes_ignore_dirs_and_missing_git() {
    let base = std::env::temp_dir().join(format!("space-git-filter-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    // 진짜 repo
    let real = base.join("proj");
    fs::create_dir_all(real.join(".git")).unwrap();
    // node_modules 하위 repo (노이즈)
    let noise = base.join("proj/node_modules/pkg");
    fs::create_dir_all(noise.join(".git")).unwrap();
    // .git 없는 디렉토리
    let plain = base.join("plain");
    fs::create_dir_all(&plain).unwrap();

    let candidates = vec![real.clone(), noise.clone(), plain.clone()];
    let repos = filter_repos(&candidates, false);
    assert!(repos.contains(&real), "{:?}", repos);
    assert!(!repos.contains(&noise), "node_modules 하위 제외: {:?}", repos);
    assert!(!repos.contains(&plain), ".git 없으면 제외: {:?}", repos);

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn old_clean_pushed_is_abandoned() {
    // 오래된(>180일) clean+pushed는 방치(정리 후보).
    let f = Fixture::new("abandoned");
    f.init();
    f.write("a.txt", "1");
    // 200일 전 커밋으로 날짜 조작.
    let old = now_secs() - 200 * 86_400;
    let date = format!("{} +0000", old);
    f.git(&["add", "-A"]);
    let ok = Command::new("git")
        .arg("-C")
        .arg(&f.dir)
        .args(["commit", "-q", "-m", "old"])
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok);
    let remote = f.dir.join("../abandoned-remote.git");
    Command::new("git")
        .args(["init", "--bare", "-q", remote.to_str().unwrap()])
        .output()
        .unwrap();
    f.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    f.git(&["push", "-q", "-u", "origin", "main"]);

    let h = probe_ok(&f.dir);
    assert!(h.last_commit_days.unwrap() > 180, "{:?}", h);
    assert_eq!(h.risk, Risk::Abandoned, "{:?}", h);
    let _ = fs::remove_dir_all(remote);
}
