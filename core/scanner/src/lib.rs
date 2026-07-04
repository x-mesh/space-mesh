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

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                ctx.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        // DirEntry::metadata는 심볼릭 링크를 따라가지 않는다 (lstat 상당).
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                ctx.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        if md.is_dir() {
            if ctx.opts.one_filesystem && md.dev() != ctx.root_dev {
                continue;
            }
            let child_name = entry.file_name().to_string_lossy().into_owned();
            subdirs.push((entry.path(), child_name, md));
        } else {
            // 하드링크는 최초 발견 시 한 번만 집계 (du와 동일).
            if md.nlink() > 1 && !ctx.seen_hardlinks.insert((md.dev(), md.ino())) {
                continue;
            }
            let alloc = md.blocks() * 512;
            logical += md.len();
            allocated += alloc;
            file_count += 1;
            ctx.total_files.fetch_add(1, Ordering::Relaxed);
            if let Some(p) = &ctx.progress {
                p.fetch_add(1, Ordering::Relaxed);
            }
            if md.len() >= ctx.opts.record_file_threshold {
                big_files.push(FileEntry {
                    path: entry.path(),
                    logical_size: md.len(),
                    allocated_size: alloc,
                });
            }
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
    all.sort_by(|a, b| b.allocated_size.cmp(&a.allocated_size));
    all.truncate(n);
    all
}

fn collect_files(node: &DirNode, out: &mut Vec<FileEntry>) {
    out.extend(node.big_files.iter().cloned());
    for c in &node.children {
        collect_files(c, out);
    }
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
