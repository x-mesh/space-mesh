//! git repo 건강도 조회 — 읽기 전용, 네트워크 없음.
//!
//! 모든 remote 판정은 로컬 remote-tracking ref(캐시) 기준이다. fetch를 하지 않으므로
//! ref가 stale할 수 있고, 그 사실을 호출자가 표시해야 한다.
//!
//! 설계(패널 리뷰 반영):
//! - unpushed는 "어느 remote에도 없는 커밋"이 아니라 **현재 브랜치 upstream(@{u}) 대비 ahead**.
//!   upstream 미설정이면 NoUpstream(위험 아님, 정보성).
//! - 위험 등급은 단일·배타, 나이 무관 신호 우선.
//! - 전역 git 동시성 상한 + repo당 command budget + 타임아웃.

use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 전역 git 프로세스 동시성 상한 — repo 수백 개에서 프로세스 폭주 방지.
static GIT_PERMITS: Semaphore = Semaphore::new();

/// repo당 git 호출 총 예산 (초). 초과가 예상되면 후순위 신호(stash/activity)를 생략.
const REPO_BUDGET: Duration = Duration::from_secs(3);
/// 개별 git 명령 타임아웃.
const CMD_TIMEOUT: Duration = Duration::from_millis(2500);

/// repo 발견 시 건너뛰는 디렉토리 이름 (이 하위의 .git은 노이즈).
const IGNORE_DIRS: &[&str] = &[
    "node_modules",
    "vendor",
    "Pods",
    ".build",
    "target",
    "dist",
    "build",
    ".next",
    "DerivedData",
];

// ───────────────────────── 동시성 세마포어 ─────────────────────────

struct Semaphore {
    count: Mutex<usize>,
    cv: Condvar,
    initialized: AtomicUsize,
}

impl Semaphore {
    const fn new() -> Self {
        Semaphore {
            count: Mutex::new(0),
            cv: Condvar::new(),
            initialized: AtomicUsize::new(0),
        }
    }
    fn permits() -> usize {
        std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1).clamp(2, 8))
            .unwrap_or(4)
    }
    fn acquire(&self) {
        let mut c = self.count.lock().unwrap();
        if self.initialized.swap(1, Ordering::SeqCst) == 0 {
            *c = Self::permits();
        }
        while *c == 0 {
            c = self.cv.wait(c).unwrap();
        }
        *c -= 1;
    }
    fn release(&self) {
        let mut c = self.count.lock().unwrap();
        *c += 1;
        self.cv.notify_one();
    }
}

struct Permit;
impl Permit {
    fn acquire() -> Permit {
        GIT_PERMITS.acquire();
        Permit
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        GIT_PERMITS.release();
    }
}

// ───────────────────────── 데이터 모델 ─────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum HeadState {
    Branch(String),
    Detached,
    /// 커밋이 하나도 없는 repo (git init 직후).
    Unborn,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpstreamState {
    /// upstream(@{u}) 대비 ahead 커밋 수 (로컬 캐시 기준).
    Ahead(u64),
    /// upstream이 있고 ahead 0.
    UpToDate,
    /// 현재 브랜치에 upstream 미설정 — 로컬 전용 브랜치. 위험 아님.
    NoUpstream,
}

/// 조회 실패 원인 — UX에서 원인별 해결 힌트를 주기 위해 분리.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeError {
    GitMissing,
    Timeout,
    PermissionDenied,
    NotARepo,
    Corrupted,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Risk {
    /// 백업 부재/유실 가능 — 나이 무관.
    Danger,
    /// remote 없음(백업 경로 없음) 또는 커밋 없음.
    Caution,
    /// clean + pushed + 오래됨 — 정리 후보.
    Abandoned,
    /// 최근 활동 + 위험 신호 없음.
    Active,
    /// 그 외 (로컬 전용 브랜치 등) — 위험 아님.
    Info,
}

#[derive(Debug, Clone)]
pub struct RepoHealth {
    pub path: PathBuf,
    pub head: HeadState,
    pub upstream: UpstreamState,
    pub has_remote: bool,
    pub tracked_dirty: u64,
    pub untracked_present: bool,
    pub stash_count: u64,
    pub last_commit_days: Option<u64>,
    /// remote-tracking ref 최신성 추정(일). None = 알 수 없음/remote 없음.
    pub remote_stale_days: Option<u64>,
    pub risk: Risk,
    /// 예산 초과로 일부 신호(stash 등)를 생략했는지.
    pub partial: bool,
}

// ───────────────────────── repo 발견 ─────────────────────────

/// 주어진 경로들 중 실제 repo 루트만 골라낸다.
/// submodule / linked worktree / known-ignore 하위는 제외.
/// `paths`는 스캔 트리에서 뽑은 ".git을 가진 디렉토리의 부모" 후보들.
pub fn filter_repos(paths: &[PathBuf], include_submodules: bool) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|p| is_real_repo(p, include_submodules))
        .cloned()
        .collect()
}

fn is_real_repo(repo: &Path, include_submodules: bool) -> bool {
    let dot_git = repo.join(".git");
    // linked worktree / submodule은 .git이 파일(gitdir 포인터).
    if dot_git.is_file() && !include_submodules {
        return false;
    }
    if !dot_git.exists() {
        return false;
    }
    // known-ignore 디렉토리 하위면 제외.
    for comp in repo.components() {
        if let std::path::Component::Normal(name) = comp {
            if let Some(s) = name.to_str() {
                if IGNORE_DIRS.contains(&s) {
                    return false;
                }
            }
        }
    }
    // 부모가 이 repo를 submodule로 등록했는지 (.gitmodules에 경로 존재).
    if !include_submodules && is_registered_submodule(repo) {
        return false;
    }
    true
}

fn is_registered_submodule(repo: &Path) -> bool {
    let Some(parent) = repo.parent() else {
        return false;
    };
    // 위로 올라가며 .gitmodules를 찾고, 그 안에 이 repo의 상대경로가 있는지.
    let mut cur = Some(parent);
    while let Some(dir) = cur {
        let modules = dir.join(".gitmodules");
        if modules.is_file() {
            if let Ok(content) = std::fs::read_to_string(&modules) {
                if let Ok(rel) = repo.strip_prefix(dir) {
                    let rel_str = rel.to_string_lossy();
                    if content
                        .lines()
                        .any(|l| l.trim_start().starts_with("path") && l.contains(rel_str.as_ref()))
                    {
                        return true;
                    }
                }
            }
            break; // 최상위 .gitmodules까지만 확인
        }
        cur = dir.parent();
    }
    false
}

// ───────────────────────── 상태 조회 ─────────────────────────

/// repo 목록의 건강 상태를 병렬 조회. 실패는 (path, error)로 분리 반환.
pub fn probe_all(repos: &[PathBuf]) -> (Vec<RepoHealth>, Vec<(PathBuf, ProbeError)>) {
    let results: Vec<Result<RepoHealth, (PathBuf, ProbeError)>> =
        repos.par_iter().map(|r| probe(r)).collect();
    let mut ok = Vec::new();
    let mut err = Vec::new();
    for r in results {
        match r {
            Ok(h) => ok.push(h),
            Err(e) => err.push(e),
        }
    }
    ok.sort_by_key(|h| risk_order(&h.risk));
    (ok, err)
}

fn risk_order(r: &Risk) -> u8 {
    match r {
        Risk::Danger => 0,
        Risk::Caution => 1,
        Risk::Abandoned => 2,
        Risk::Active => 3,
        Risk::Info => 4,
    }
}

/// 단일 repo 조회. 전역 동시성 상한 하에서 실행, repo당 예산 초과 시 후순위 신호 생략.
pub fn probe(repo: &Path) -> Result<RepoHealth, (PathBuf, ProbeError)> {
    let _permit = Permit::acquire();
    let started = SystemTime::now();

    // git status --porcelain=v2 --branch 한 번으로 head·upstream·ahead·dirty·untracked를 얻는다.
    let status = match git(
        repo,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ],
    ) {
        GitOut::Ok(s) => s,
        GitOut::Fail(msg) => {
            if msg.contains("Permission denied") {
                return Err((repo.into(), ProbeError::PermissionDenied));
            }
            if msg.contains("not a git repository") {
                return Err((repo.into(), ProbeError::NotARepo));
            }
            return Err((repo.into(), ProbeError::Corrupted));
        }
        GitOut::Timeout => return Err((repo.into(), ProbeError::Timeout)),
        GitOut::Missing => return Err((repo.into(), ProbeError::GitMissing)),
    };

    let mut head_name: Option<String> = None;
    let mut is_unborn = false;
    let mut has_upstream = false;
    let mut ahead: u64 = 0;
    let mut tracked_dirty: u64 = 0;
    let mut untracked_present = false;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("# branch.oid ") {
            is_unborn = rest.trim() == "(initial)";
        } else if let Some(rest) = line.strip_prefix("# branch.head ") {
            let h = rest.trim();
            if h != "(detached)" {
                head_name = Some(h.to_string());
            }
        } else if line.starts_with("# branch.upstream ") {
            has_upstream = true;
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            if let Some(a) = rest.split_whitespace().next() {
                ahead = a.trim_start_matches('+').parse().unwrap_or(0);
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ") {
            tracked_dirty += 1; // changed / renamed / unmerged tracked entry
        } else if line.starts_with("? ") {
            untracked_present = true;
        }
    }

    let head = if is_unborn {
        HeadState::Unborn
    } else if let Some(name) = head_name {
        HeadState::Branch(name)
    } else {
        HeadState::Detached
    };

    let upstream = if !has_upstream || matches!(head, HeadState::Unborn | HeadState::Detached) {
        UpstreamState::NoUpstream
    } else if ahead > 0 {
        UpstreamState::Ahead(ahead)
    } else {
        UpstreamState::UpToDate
    };

    let has_remote = matches!(git(repo, &["remote"]), GitOut::Ok(s) if !s.trim().is_empty());

    let last_commit_days = match &head {
        HeadState::Unborn => None,
        _ => match git(repo, &["log", "-1", "--format=%ct"]) {
            GitOut::Ok(s) => s
                .trim()
                .parse::<u64>()
                .ok()
                .map(|ct| now_secs().saturating_sub(ct) / 86_400),
            _ => None,
        },
    };

    // 예산 내면 stash도 조회 (방치 오분류 방지의 핵심 신호).
    let mut partial = false;
    let stash_count = if started.elapsed().unwrap_or(REPO_BUDGET) < REPO_BUDGET {
        match git(repo, &["stash", "list"]) {
            GitOut::Ok(s) => s.lines().filter(|l| !l.is_empty()).count() as u64,
            _ => 0,
        }
    } else {
        partial = true;
        0
    };

    let remote_stale_days = if has_remote {
        fetch_head_age_days(repo)
    } else {
        None
    };

    let risk = classify(
        &head,
        &upstream,
        has_remote,
        tracked_dirty,
        untracked_present,
        stash_count,
        last_commit_days,
    );

    Ok(RepoHealth {
        path: repo.into(),
        head,
        upstream,
        has_remote,
        tracked_dirty,
        untracked_present,
        stash_count,
        last_commit_days,
        remote_stale_days,
        risk,
        partial,
    })
}

/// 위험 등급 — 단일·배타, 평가 순서대로 첫 매치. 나이 무관 신호 우선.
fn classify(
    head: &HeadState,
    upstream: &UpstreamState,
    has_remote: bool,
    tracked_dirty: u64,
    untracked_present: bool,
    stash_count: u64,
    last_commit_days: Option<u64>,
) -> Risk {
    let ahead_with_upstream = matches!(upstream, UpstreamState::Ahead(_));
    let detached_with_commits = *head == HeadState::Detached;

    // 1. 위험: 유실 가능한 실제 작업이 있음 (나이 무관).
    //    tracked 변경/stash/ahead/detached — untracked만으로는 위험 아님(개발 repo에 흔한
    //    빌드 산출물·신규 파일이라 오탐이 큼. 패널: tracked/untracked 분리).
    if ahead_with_upstream || tracked_dirty > 0 || stash_count > 0 || detached_with_commits {
        return Risk::Danger;
    }
    // 2. 주의: untracked만 있거나(add 안 한 신규 파일 가능), 백업 경로 없음, 커밋 없음.
    if untracked_present || !has_remote || *head == HeadState::Unborn {
        return Risk::Caution;
    }
    // 3. 방치: clean + pushed + 오래됨 (정리 후보). stash 없음은 위에서 보장.
    if let Some(days) = last_commit_days {
        if days > 180 {
            return Risk::Abandoned;
        }
        // 4. 활성.
        if days <= 14 {
            return Risk::Active;
        }
    }
    // 5. 그 외 (로컬 전용 브랜치 no-upstream, 15~180일 등).
    Risk::Info
}

// ───────────────────────── git 실행 유틸 ─────────────────────────

enum GitOut {
    Ok(String),
    Fail(String),
    Timeout,
    Missing,
}

fn git(repo: &Path, args: &[&str]) -> GitOut {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // 인증 프롬프트/에디터 차단 (네트워크·상호작용 없음).
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_OPTIONAL_LOCKS", "0"); // status가 index.lock 잡지 않게

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return GitOut::Missing,
        Err(_) => return GitOut::Fail("spawn failed".into()),
    };

    // 타임아웃 폴링.
    let start = SystemTime::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = child.wait_with_output().ok();
                let stdout = out
                    .as_ref()
                    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                    .unwrap_or_default();
                let stderr = out
                    .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
                    .unwrap_or_default();
                return if status.success() {
                    GitOut::Ok(stdout)
                } else {
                    GitOut::Fail(stderr)
                };
            }
            Ok(None) => {
                if start.elapsed().unwrap_or(CMD_TIMEOUT) >= CMD_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return GitOut::Timeout;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return GitOut::Fail("wait failed".into()),
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// .git/FETCH_HEAD mtime으로 마지막 fetch 이후 일수 추정 (remote-tracking 최신성).
fn fetch_head_age_days(repo: &Path) -> Option<u64> {
    let fh = repo.join(".git/FETCH_HEAD");
    let meta = std::fs::metadata(&fh).ok()?;
    let mtime = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(mtime).ok()?;
    Some(age.as_secs() / 86_400)
}

/// git 상태 캐시 무효화 signature — .git 내부 상태 파일들의 최대 mtime(ns).
/// HEAD(브랜치/커밋), index(staging), ORIG_HEAD·MERGE_HEAD, packed-refs(ref 갱신)가
/// 바뀌면 값이 변한다. working-tree 변경(단순 파일 수정)은 여기 안 잡히므로,
/// 호출자가 스캔 트리의 (file_count, allocated_size)를 보조 키로 결합해 무효화한다.
pub fn git_signature(repo: &Path) -> u64 {
    let git_dir = repo.join(".git");
    let mut max_ns: u64 = 0;
    for name in ["HEAD", "index", "ORIG_HEAD", "MERGE_HEAD", "packed-refs"] {
        if let Ok(meta) = std::fs::metadata(git_dir.join(name)) {
            if let Ok(mtime) = meta.modified() {
                if let Ok(dur) = mtime.duration_since(UNIX_EPOCH) {
                    max_ns = max_ns.max(dur.as_nanos() as u64);
                }
            }
        }
    }
    // refs/heads 디렉토리 mtime (loose ref 갱신).
    if let Ok(meta) = std::fs::metadata(git_dir.join("refs/heads")) {
        if let Ok(mtime) = meta.modified() {
            if let Ok(dur) = mtime.duration_since(UNIX_EPOCH) {
                max_ns = max_ns.max(dur.as_nanos() as u64);
            }
        }
    }
    max_ns
}

/// 최근 `weeks`주 주별 커밋 수 (activity 스파크라인). lazy 호출용.
pub fn activity(repo: &Path, weeks: u64) -> Vec<u64> {
    let _permit = Permit::acquire();
    let since = now_secs().saturating_sub(weeks * 7 * 86_400);
    let mut buckets = vec![0u64; weeks as usize];
    if let GitOut::Ok(s) = git(
        repo,
        &[
            "log",
            "--format=%ct",
            &format!("--since={}", since),
            "--all",
        ],
    ) {
        for line in s.lines() {
            if let Ok(ct) = line.trim().parse::<u64>() {
                if ct >= since {
                    let weeks_ago = (now_secs().saturating_sub(ct)) / (7 * 86_400);
                    let idx = (weeks - 1).saturating_sub(weeks_ago.min(weeks - 1)) as usize;
                    buckets[idx] += 1;
                }
            }
        }
    }
    buckets
}

#[cfg(test)]
mod tests;
