//! 병렬 디스크 스캔 엔진.
//!
//! rayon work-stealing으로 디렉토리 트리를 병렬 순회하고,
//! logical size(st_size)와 allocated size(st_blocks * 512)를 함께 집계한다.
//! 하드링크(nlink > 1)는 (dev, ino) 기준으로 한 번만 계산해 du와 동일한 기준을 따른다.

use dashmap::DashSet;
use rayon::prelude::*;
use serde::Serialize;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// 이 개수 이상 항목을 가진 청크는 lstat을 병렬로 수행한다 (PERF-003A).
/// 소형 디렉토리는 rayon 오버헤드가 syscall 절약보다 커서 순차가 빠르다.
const PAR_LSTAT_MIN_ENTRIES: usize = 512;

/// readdir 청크 크기 — DirEntry(항목당 수백 B)는 이 개수 이상 쌓이지 않는다.
/// 대형 디렉토리도 청크 단위로 lstat→축약을 반복해 peak RSS에 상한을 건다.
const PAR_LSTAT_CHUNK: usize = 4096;

/// 스캔 옵션.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// 이 크기(bytes) 이상인 파일은 개별 FileEntry로 기록한다.
    pub record_file_threshold: u64,
    /// true면 루트와 다른 파일시스템(device)으로 내려가지 않는다 (du -x 상당).
    pub one_filesystem: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            record_file_threshold: 50 * 1024 * 1024, // 50 MiB
            one_filesystem: false,
        }
    }
}

/// 디렉토리 노드. children은 하위 디렉토리만 담는다.
#[derive(Debug, Serialize)]
pub struct DirNode {
    pub name: String,
    pub logical_size: u64,
    pub allocated_size: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub children: Vec<DirNode>,
    /// record_file_threshold 이상인 이 디렉토리 직속 파일들.
    pub big_files: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub logical_size: u64,
    pub allocated_size: u64,
    /// 마지막 수정 시각 (unix epoch 초). 스캔 시 확보한 Metadata에서 읽어
    /// 추가 syscall 없이 채운다. 0 = 알 수 없음.
    pub modified_epoch: i64,
}

/// 스캔 통계 (트리와 별도로 수집).
#[derive(Debug, Default)]
pub struct ScanStats {
    /// 읽기 실패(권한 등)로 건너뛴 항목 수.
    pub errors: u64,
    pub total_files: u64,
    pub total_dirs: u64,
}

pub struct ScanResult {
    pub root: DirNode,
    pub stats: ScanStats,
}

/// 집계에 필요한 최소 파일 메타 — lstat 결과에서 즉시 축약해 DirEntry를 버린다.
/// path는 record_file_threshold 이상 파일만 보유한다 (소형 파일은 힙 할당 0).
struct FileMeta {
    logical: u64,
    alloc: u64,
    dev: u64,
    ino: u64,
    nlink: u64,
    mtime: i64,
    path: Option<PathBuf>,
}

/// lstat 직후의 항목 분류 결과. Dir은 재귀에 Metadata가 필요해 유지한다
/// (디렉토리 수는 파일 수보다 한 자릿수 이상 적다).
enum Scanned {
    Dir(PathBuf, String, fs::Metadata),
    File(FileMeta),
}

fn classify(entry: fs::DirEntry, md: fs::Metadata, threshold: u64) -> Scanned {
    if md.is_dir() {
        let name = entry.file_name().to_string_lossy().into_owned();
        Scanned::Dir(entry.path(), name, md)
    } else {
        Scanned::File(FileMeta {
            logical: md.len(),
            alloc: md.blocks() * 512,
            dev: md.dev(),
            ino: md.ino(),
            nlink: md.nlink(),
            mtime: md.mtime(),
            path: (md.len() >= threshold).then(|| entry.path()),
        })
    }
}

struct Ctx {
    opts: ScanOptions,
    root_dev: u64,
    /// nlink > 1 파일의 (dev, ino) — 최초 1회만 크기 집계.
    seen_hardlinks: DashSet<(u64, u64)>,
    errors: AtomicU64,
    total_files: AtomicU64,
    total_dirs: AtomicU64,
    /// 외부(UI)에서 폴링하는 라이브 진행 카운터 (스캔한 파일 수).
    progress: Option<Arc<AtomicU64>>,
}

/// 루트 경로를 병렬 스캔해 디렉토리 트리를 반환한다.
pub fn scan(root: &Path, opts: ScanOptions) -> std::io::Result<ScanResult> {
    scan_with_progress(root, opts, None)
}

/// scan()과 동일하되, 진행 상황을 외부 AtomicU64로 실시간 보고한다.
pub fn scan_with_progress(
    root: &Path,
    opts: ScanOptions,
    progress: Option<Arc<AtomicU64>>,
) -> std::io::Result<ScanResult> {
    let root_md = fs::symlink_metadata(root)?;
    if !root_md.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "scan root must be a directory",
        ));
    }
    let ctx = Ctx {
        opts,
        root_dev: root_md.dev(),
        seen_hardlinks: DashSet::new(),
        errors: AtomicU64::new(0),
        total_files: AtomicU64::new(0),
        total_dirs: AtomicU64::new(0),
        progress,
    };
    let name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    let node = scan_dir(root, name, &root_md, &ctx);
    Ok(ScanResult {
        root: node,
        stats: ScanStats {
            errors: ctx.errors.load(Ordering::Relaxed),
            total_files: ctx.total_files.load(Ordering::Relaxed),
            total_dirs: ctx.total_dirs.load(Ordering::Relaxed),
        },
    })
}

fn scan_dir(path: &Path, name: String, dir_md: &fs::Metadata, ctx: &Ctx) -> DirNode {
    ctx.total_dirs.fetch_add(1, Ordering::Relaxed);

    // 디렉토리 자체가 점유하는 블록도 du처럼 포함한다.
    let mut logical = 0u64;
    let mut allocated = dir_md.blocks() * 512;
    let mut file_count = 0u64;
    let mut big_files = Vec::new();
    let mut subdirs: Vec<(PathBuf, String, fs::Metadata)> = Vec::new();

    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => {
            ctx.errors.fetch_add(1, Ordering::Relaxed);
            return DirNode {
                name,
                logical_size: logical,
                allocated_size: allocated,
                file_count: 0,
                dir_count: 1,
                children: Vec::new(),
                big_files,
            };
        }
    };

    // ①+② readdir을 청크 단위로 소비하며 lstat→축약(classify)을 반복한다.
    // - 큰 청크(≥512)는 lstat 병렬 (PERF-003A) — 하위 디렉토리 병렬 재귀가
    //   못 덮는 "한 디렉토리에 파일 수만 개" 케이스를 잡는다.
    // - DirEntry(항목당 수백 B + 이름 힙)는 청크 버퍼(≤4096) 이상 쌓이지 않고,
    //   축약된 Scanned(~80B, 소형 파일은 힙 0)만 남긴다 — (DirEntry, Metadata)
    //   쌍을 디렉토리 전량 보유하면 대형 트리에서 peak RSS가 GB급으로 치솟는다
    //   (실측 71MB → 1.98GB 회귀를 되돌린 수정).
    // - DirEntry::metadata는 심볼릭 링크를 따라가지 않는다 (lstat 상당).
    let threshold = ctx.opts.record_file_threshold;
    let mut entries = entries;
    let mut buf: Vec<fs::DirEntry> = Vec::new();
    let mut chunk: Vec<Scanned> = Vec::new();
    loop {
        for entry in entries.by_ref() {
            match entry {
                Ok(e) => {
                    buf.push(e);
                    if buf.len() >= PAR_LSTAT_CHUNK {
                        break;
                    }
                }
                Err(_) => {
                    ctx.errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if buf.is_empty() {
            break;
        }
        if buf.len() >= PAR_LSTAT_MIN_ENTRIES {
            let results: Vec<Option<Scanned>> = buf
                .par_drain(..)
                .map(|e| {
                    let md = e.metadata().ok()?;
                    Some(classify(e, md, threshold))
                })
                .collect();
            let failed = results.iter().filter(|r| r.is_none()).count() as u64;
            if failed > 0 {
                ctx.errors.fetch_add(failed, Ordering::Relaxed);
            }
            chunk.extend(results.into_iter().flatten());
        } else {
            for e in buf.drain(..) {
                match e.metadata() {
                    Ok(md) => chunk.push(classify(e, md, threshold)),
                    Err(_) => {
                        ctx.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        // ③ 집계 — 청크 즉시 소비 (축약 항목도 디렉토리 전량을 들고 있지 않는다).
        for item in chunk.drain(..) {
            match item {
                Scanned::Dir(child_path, child_name, md) => {
                    if ctx.opts.one_filesystem && md.dev() != ctx.root_dev {
                        continue;
                    }
                    subdirs.push((child_path, child_name, md));
                }
                Scanned::File(f) => {
                    // 하드링크는 최초 발견 시 한 번만 집계 (du와 동일).
                    if f.nlink > 1 && !ctx.seen_hardlinks.insert((f.dev, f.ino)) {
                        continue;
                    }
                    logical += f.logical;
                    allocated += f.alloc;
                    file_count += 1;
                    if let Some(path) = f.path {
                        big_files.push(FileEntry {
                            path,
                            logical_size: f.logical,
                            allocated_size: f.alloc,
                            modified_epoch: f.mtime,
                        });
                    }
                }
            }
        }
    }

    // 원자 카운터는 파일당이 아니라 디렉토리당 1회 가산 — 멀티코어 스캔의
    // 공유 캐시라인 경합을 줄인다 (PERF-004). progress는 UI 폴링용이라
    // 디렉토리 단위 granularity로 충분하다.
    if file_count > 0 {
        ctx.total_files.fetch_add(file_count, Ordering::Relaxed);
        if let Some(p) = &ctx.progress {
            p.fetch_add(file_count, Ordering::Relaxed);
        }
    }

    let children: Vec<DirNode> = subdirs
        .into_par_iter()
        .map(|(p, n, md)| scan_dir(&p, n, &md, ctx))
        .collect();

    let mut dir_count = 1u64;
    for c in &children {
        logical += c.logical_size;
        allocated += c.allocated_size;
        file_count += c.file_count;
        dir_count += c.dir_count;
    }

    DirNode {
        name,
        logical_size: logical,
        allocated_size: allocated,
        file_count,
        dir_count,
        children,
        big_files,
    }
}

/// 트리 전체에서 가장 큰 파일 top-N을 수집한다.
pub fn top_files(root: &DirNode, n: usize) -> Vec<FileEntry> {
    let mut all = Vec::new();
    collect_files(root, &mut all);
    all.sort_by_key(|f| std::cmp::Reverse(f.allocated_size));
    all.truncate(n);
    all
}

fn collect_files(node: &DirNode, out: &mut Vec<FileEntry>) {
    out.extend(node.big_files.iter().cloned());
    for c in &node.children {
        collect_files(c, out);
    }
}

/// modified_epoch 기준 경과일수 (미래 mtime이면 음수).
pub fn age_days(modified_epoch: i64, now_epoch: i64) -> i64 {
    (now_epoch - modified_epoch) / 86_400
}

/// 방치 점수 — allocated_size × 경과일 (곱 오버플로 방지 u128).
/// 크기와 방치 기간 모두에 비례하는, 설명 가능한 단순 랭킹.
pub fn stale_score(f: &FileEntry, now_epoch: i64) -> u128 {
    let days = age_days(f.modified_epoch, now_epoch).max(0) as u128;
    f.allocated_size as u128 * days
}

/// 트리 전체에서 "크고 오래 방치된" 파일 top-N (점수 내림차순).
/// mtime을 모르거나(0) min_age_days 미만인 파일은 제외한다.
pub fn stale_files(
    root: &DirNode,
    n: usize,
    min_age_days: u64,
    now_epoch: i64,
) -> Vec<FileEntry> {
    let mut all = Vec::new();
    collect_files(root, &mut all);
    all.retain(|f| {
        f.modified_epoch > 0 && age_days(f.modified_epoch, now_epoch) >= min_age_days as i64
    });
    all.sort_by_key(|f| std::cmp::Reverse(stale_score(f, now_epoch)));
    all.truncate(n);
    all
}

/// JSON 직렬화 전에 트리를 max_depth까지만 남기고 잘라낸다 (집계값은 유지).
pub fn truncate_depth(node: &mut DirNode, max_depth: usize) {
    if max_depth == 0 {
        node.children.clear();
        return;
    }
    for c in &mut node.children {
        truncate_depth(c, max_depth - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write as _;

    fn write_file(path: &Path, bytes: usize) {
        let mut f = File::create(path).unwrap();
        f.write_all(&vec![0xABu8; bytes]).unwrap();
    }

    #[test]
    fn aggregates_sizes_and_counts() {
        let tmp = std::env::temp_dir().join(format!("space-mesh-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("sub")).unwrap();
        write_file(&tmp.join("a.bin"), 10_000);
        write_file(&tmp.join("sub/b.bin"), 20_000);

        let result = scan(&tmp, ScanOptions::default()).unwrap();
        assert_eq!(result.stats.total_files, 2);
        assert_eq!(result.stats.total_dirs, 2);
        assert_eq!(result.root.logical_size, 30_000);
        // allocated는 블록 단위 올림이므로 logical 이상이어야 한다.
        assert!(result.root.allocated_size >= 30_000);

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn hardlinks_counted_once() {
        let tmp = std::env::temp_dir().join(format!("space-mesh-hl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        write_file(&tmp.join("orig.bin"), 40_000);
        fs::hard_link(tmp.join("orig.bin"), tmp.join("link.bin")).unwrap();

        let result = scan(&tmp, ScanOptions::default()).unwrap();
        // 하드링크 쌍은 한 번만 집계된다.
        assert_eq!(result.stats.total_files, 1);
        assert_eq!(result.root.logical_size, 40_000);

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn flat_dir_above_par_threshold_counts_correctly() {
        // PAR_LSTAT_MIN_ENTRIES(512) 초과 평면 디렉토리 — 병렬 lstat 경로 검증.
        let tmp = std::env::temp_dir().join(format!("space-mesh-flat-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let n = PAR_LSTAT_MIN_ENTRIES + 88;
        for i in 0..n {
            write_file(&tmp.join(format!("f{:04}.bin", i)), 100);
        }
        // 하드링크 쌍 하나 — 병렬 경로에서도 1회만 집계되는지 확인.
        fs::hard_link(tmp.join("f0000.bin"), tmp.join("hardlink.bin")).unwrap();

        let result = scan(&tmp, ScanOptions::default()).unwrap();
        assert_eq!(result.stats.total_files, n as u64); // 링크는 +1 안 됨
        assert_eq!(result.root.logical_size, (n * 100) as u64);
        assert_eq!(result.stats.errors, 0);

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn stale_files_ranked_by_size_times_age() {
        let tmp = std::env::temp_dir().join(format!("space-mesh-stale-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        write_file(&tmp.join("fresh-big.bin"), 100_000);
        write_file(&tmp.join("old-small.bin"), 60_000);
        write_file(&tmp.join("old-big.bin"), 90_000);

        // old-* 파일의 mtime을 과거로 되돌린다 (400일 / 200일 전).
        let day = std::time::Duration::from_secs(86_400);
        let set_age = |name: &str, days: u64| {
            let f = File::options().write(true).open(tmp.join(name)).unwrap();
            f.set_modified(std::time::SystemTime::now() - day * days as u32)
                .unwrap();
        };
        set_age("old-small.bin", 400);
        set_age("old-big.bin", 200);

        let opts = ScanOptions {
            record_file_threshold: 50_000,
            ..Default::default()
        };
        let result = scan(&tmp, opts).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // 신선한 파일은 min_age_days에 걸러지고, 점수(크기×방치일) 내림차순이어야 한다.
        let stale = stale_files(&result.root, 10, 30, now);
        assert_eq!(stale.len(), 2, "{:?}", stale);
        // old-small: 60KB×400d = 24M, old-big: 90KB×200d = 18M → old-small이 1위.
        assert!(stale[0].path.ends_with("old-small.bin"), "{:?}", stale[0].path);
        assert!(stale[1].path.ends_with("old-big.bin"));
        assert!(stale[0].modified_epoch > 0);
        assert!(age_days(stale[0].modified_epoch, now) >= 399);
    }

    #[test]
    fn big_files_recorded_above_threshold() {
        let tmp = std::env::temp_dir().join(format!("space-mesh-big-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        write_file(&tmp.join("small.bin"), 1_000);
        write_file(&tmp.join("big.bin"), 100_000);

        let opts = ScanOptions {
            record_file_threshold: 50_000,
            ..Default::default()
        };
        let result = scan(&tmp, opts).unwrap();
        let top = top_files(&result.root, 10);
        assert_eq!(top.len(), 1);
        assert!(top[0].path.ends_with("big.bin"));

        fs::remove_dir_all(&tmp).unwrap();
    }
}
