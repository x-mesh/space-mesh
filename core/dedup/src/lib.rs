//! 3단 계층 필터 중복 파일 탐지.
//!
//! ① 크기 그룹핑(유일 크기는 즉시 탈락) → ② 헤드+테일 4KiB 부분 해시 →
//! ③ 생존 후보만 blake3 전체 해시. 하드링크는 scanner가 기록 단계에서 이미 1회로
//! 접어두므로 같은 그룹에 중복 등장하지 않는다.

use rayon::prelude::*;
use space_scanner::{scan_with_progress, DirNode, FileEntry, ScanOptions};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

const PARTIAL_CHUNK: usize = 4096;

#[derive(Debug, Clone)]
pub struct DupGroup {
    /// 그룹 내 파일 한 개의 크기 (모두 동일).
    pub file_size: u64,
    /// 하나만 남기고 지웠을 때 회수되는 공간.
    pub reclaimable: u64,
    pub hash_hex: String,
    /// 첫 번째 = 보존 추천본(최신 mtime, 동률이면 경로순 첫 파일).
    /// 나머지는 경로순 — UI/CLI의 "첫 파일만 남기고 정리" 관례와 결합된다.
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct DedupStats {
    pub candidates: u64,
    pub partial_hashed: u64,
    pub full_hashed: u64,
}

pub struct DedupResult {
    pub groups: Vec<DupGroup>,
    pub stats: DedupStats,
}

/// root 아래에서 min_size 이상인 파일들의 중복 그룹을 찾는다.
/// progress는 해시 처리한 파일 수를 실시간 보고한다.
pub fn find_duplicates(
    root: &Path,
    min_size: u64,
    progress: Option<Arc<AtomicU64>>,
) -> std::io::Result<DedupResult> {
    // scanner의 big_files 기록(threshold = min_size)을 후보 수집기로 재사용.
    let result = scan_with_progress(
        root,
        ScanOptions {
            record_file_threshold: min_size.max(1),
            one_filesystem: false,
        },
        None,
    )?;
    let mut files: Vec<FileEntry> = Vec::new();
    collect(&result.root, &mut files);
    Ok(dedup_candidates(files, progress))
}

/// 이미 스캔된 트리를 후보 수집기로 재사용하는 변형 — 재스캔 없이 즉시
/// 해시 파이프라인으로 들어간다 (PERF-001).
///
/// 주의: 트리에는 스캔 당시 record_file_threshold 이상 파일만 기록돼 있다.
/// min_size가 그보다 작아도 그 사이 구간 파일은 애초에 트리에 없으므로,
/// 호출자가 "min_size >= 스캔 임계"일 때만 이 경로를 택해야 한다.
pub fn find_duplicates_in_tree(
    root: &DirNode,
    subroot: Option<&Path>,
    min_size: u64,
    progress: Option<Arc<AtomicU64>>,
) -> DedupResult {
    let mut files: Vec<FileEntry> = Vec::new();
    collect(root, &mut files);
    files.retain(|f| f.logical_size >= min_size);
    if let Some(sub) = subroot {
        files.retain(|f| f.path.starts_with(sub));
    }
    dedup_candidates(files, progress)
}

/// 후보 목록 → ①크기 그룹핑 → ②부분 해시 → ③전체 해시 파이프라인.
fn dedup_candidates(files: Vec<FileEntry>, progress: Option<Arc<AtomicU64>>) -> DedupResult {
    // ① 크기 그룹핑.
    let mut by_size: std::collections::HashMap<u64, Vec<FileEntry>> =
        std::collections::HashMap::new();
    for f in files {
        by_size.entry(f.logical_size).or_default().push(f);
    }
    by_size.retain(|_, v| v.len() > 1);
    let candidates: u64 = by_size.values().map(|v| v.len() as u64).sum();

    // ② 같은 크기 그룹 내 부분 해시(헤드+테일 4KiB).
    // 그룹 단위 + 그룹 내부 이중 병렬 — 같은 크기 파일이 수백 개인 그룹이
    // 단일 스레드에 갇히지 않게 한다 (PERF-007).
    let partial_groups: Vec<Vec<FileEntry>> = by_size
        .into_par_iter()
        .flat_map(|(size, group)| {
            let hashed: Vec<([u8; 32], FileEntry)> = group
                .into_par_iter()
                .filter_map(|entry| {
                    let h = partial_hash(&entry.path, size).ok()?;
                    if let Some(p) = &progress {
                        p.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Some((h, entry))
                })
                .collect();
            let mut by_partial: std::collections::HashMap<[u8; 32], Vec<FileEntry>> =
                std::collections::HashMap::new();
            for (h, entry) in hashed {
                by_partial.entry(h).or_default().push(entry);
            }
            by_partial
                .into_values()
                .filter(|v| v.len() > 1)
                .collect::<Vec<_>>()
        })
        .collect();
    let partial_hashed: u64 = partial_groups.iter().map(|g| g.len() as u64).sum();

    // ③ 생존 후보만 전체 blake3 해시 (그룹 내부도 병렬 — PERF-007).
    let groups: Vec<DupGroup> = partial_groups
        .into_par_iter()
        .flat_map(|group| {
            let hashed: Vec<(blake3::Hash, FileEntry)> = group
                .into_par_iter()
                .filter_map(|entry| {
                    let h = full_hash(&entry.path).ok()?;
                    if let Some(p) = &progress {
                        p.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Some((h, entry))
                })
                .collect();
            let mut by_full: std::collections::HashMap<blake3::Hash, Vec<FileEntry>> =
                std::collections::HashMap::new();
            for (h, entry) in hashed {
                by_full.entry(h).or_default().push(entry);
            }
            by_full
                .into_iter()
                .filter(|(_, v)| v.len() > 1)
                .map(|(hash, mut v)| {
                    v.sort_by(|a, b| a.path.cmp(&b.path));
                    // Smart Keep: 최신 수정본을 맨 앞(보존 추천)으로.
                    // 동률이면 경로순 첫 파일 유지 (결정적 순서).
                    let mut keep = 0;
                    for (i, e) in v.iter().enumerate().skip(1) {
                        if e.modified_epoch > v[keep].modified_epoch {
                            keep = i;
                        }
                    }
                    if keep > 0 {
                        let e = v.remove(keep);
                        v.insert(0, e);
                    }
                    let size = v[0].logical_size;
                    DupGroup {
                        file_size: size,
                        reclaimable: v[0].allocated_size * (v.len() as u64 - 1),
                        hash_hex: hash.to_hex().to_string(),
                        files: v.into_iter().map(|e| e.path).collect(),
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let full_hashed: u64 = groups.iter().map(|g| g.files.len() as u64).sum();
    let mut groups = groups;
    groups.sort_by_key(|g| std::cmp::Reverse(g.reclaimable));
    DedupResult {
        groups,
        stats: DedupStats {
            candidates,
            partial_hashed,
            full_hashed,
        },
    }
}

fn collect(node: &DirNode, out: &mut Vec<FileEntry>) {
    out.extend(node.big_files.iter().cloned());
    for c in &node.children {
        collect(c, out);
    }
}

fn partial_hash(path: &Path, size: u64) -> std::io::Result<[u8; 32]> {
    let mut f = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; PARTIAL_CHUNK];

    let n = f.read(&mut buf)?;
    hasher.update(&buf[..n]);
    if size > (PARTIAL_CHUNK * 2) as u64 {
        f.seek(SeekFrom::End(-(PARTIAL_CHUNK as i64)))?;
        let n = f.read(&mut buf)?;
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn full_hash(path: &Path) -> std::io::Result<blake3::Hash> {
    // mmap + rayon: 파일이 크면 blake3가 내부적으로 청크 병렬 해시한다 (PERF-002).
    // 작은 파일은 blake3가 알아서 일반 read로 폴백한다.
    let mut hasher = blake3::Hasher::new();
    hasher.update_mmap_rayon(path)?;
    Ok(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_exact_duplicates_only() {
        let tmp = std::env::temp_dir().join(format!("space-dedup-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("a")).unwrap();
        fs::create_dir_all(tmp.join("b")).unwrap();

        // 동일 내용 3벌, 크기만 같은 다른 내용 1개, 유일 크기 1개.
        let content = vec![7u8; 20_000];
        fs::write(tmp.join("a/dup1.bin"), &content).unwrap();
        fs::write(tmp.join("b/dup2.bin"), &content).unwrap();
        fs::write(tmp.join("b/dup3.bin"), &content).unwrap();
        let mut different = content.clone();
        different[10_000] = 99; // 같은 크기, 중간만 다름 → 부분해시 통과, 전체해시에서 갈림
        fs::write(tmp.join("a/same-size-diff.bin"), &different).unwrap();
        fs::write(tmp.join("a/unique.bin"), vec![1u8; 5_000]).unwrap();

        let result = find_duplicates(&tmp, 1, None).unwrap();
        assert_eq!(result.groups.len(), 1, "{:?}", result.groups);
        let g = &result.groups[0];
        assert_eq!(g.files.len(), 3);
        assert_eq!(g.file_size, 20_000);
        assert!(g.reclaimable >= 40_000);

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn in_tree_matches_full_rescan() {
        let tmp = std::env::temp_dir().join(format!("space-dedup-tree-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("a")).unwrap();
        fs::create_dir_all(tmp.join("b")).unwrap();
        let content = vec![5u8; 25_000];
        fs::write(tmp.join("a/dup1.bin"), &content).unwrap();
        fs::write(tmp.join("b/dup2.bin"), &content).unwrap();
        fs::write(tmp.join("a/unique.bin"), vec![6u8; 30_000]).unwrap();

        // 재스캔 경로와 트리 재사용 경로가 같은 그룹을 내야 한다.
        let rescan = find_duplicates(&tmp, 1, None).unwrap();
        let scanned = space_scanner::scan(
            &tmp,
            ScanOptions {
                record_file_threshold: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let in_tree = find_duplicates_in_tree(&scanned.root, None, 1, None);
        assert_eq!(rescan.groups.len(), 1);
        assert_eq!(in_tree.groups.len(), 1);
        assert_eq!(in_tree.groups[0].files, rescan.groups[0].files);
        assert_eq!(in_tree.groups[0].hash_hex, rescan.groups[0].hash_hex);

        // subroot 필터: b/ 아래만 보면 중복 그룹이 없어야 한다 (한 벌뿐).
        let only_b = find_duplicates_in_tree(&scanned.root, Some(&tmp.join("b")), 1, None);
        assert!(only_b.groups.is_empty(), "{:?}", only_b.groups);

        // min_size 필터: 25KB 초과 임계면 dup 쌍이 걸러진다.
        let too_big = find_duplicates_in_tree(&scanned.root, None, 26_000, None);
        assert!(too_big.groups.is_empty());

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn smart_keep_puts_newest_first() {
        let tmp = std::env::temp_dir().join(format!("space-dedup-keep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // 경로순으로는 a-old가 앞서지만, z-newest가 최신이므로 보존 추천 1순위여야 한다.
        let content = vec![9u8; 15_000];
        fs::write(tmp.join("a-old.bin"), &content).unwrap();
        fs::write(tmp.join("m-older.bin"), &content).unwrap();
        fs::write(tmp.join("z-newest.bin"), &content).unwrap();
        let day = std::time::Duration::from_secs(86_400);
        let set_age = |name: &str, days: u32| {
            let f = fs::File::options()
                .write(true)
                .open(tmp.join(name))
                .unwrap();
            f.set_modified(std::time::SystemTime::now() - day * days)
                .unwrap();
        };
        set_age("a-old.bin", 100);
        set_age("m-older.bin", 300);

        let result = find_duplicates(&tmp, 1, None).unwrap();
        assert_eq!(result.groups.len(), 1, "{:?}", result.groups);
        let files = &result.groups[0].files;
        assert!(files[0].ends_with("z-newest.bin"), "{:?}", files);
        // 나머지는 경로순 유지.
        assert!(files[1].ends_with("a-old.bin"));
        assert!(files[2].ends_with("m-older.bin"));

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn hardlinks_not_reported_as_duplicates() {
        let tmp = std::env::temp_dir().join(format!("space-dedup-hl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("orig.bin"), vec![3u8; 30_000]).unwrap();
        fs::hard_link(tmp.join("orig.bin"), tmp.join("hl.bin")).unwrap();

        let result = find_duplicates(&tmp, 1, None).unwrap();
        assert!(result.groups.is_empty(), "{:?}", result.groups);

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn middle_difference_detected_despite_partial_hash() {
        // 헤드/테일이 같고 중간만 다른 대용량 파일 — 부분해시는 충돌하지만
        // 전체해시가 걸러내야 한다.
        let tmp = std::env::temp_dir().join(format!("space-dedup-mid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let mut a = vec![0u8; 100_000];
        let mut b = vec![0u8; 100_000];
        a[50_000] = 1;
        b[50_000] = 2;
        fs::write(tmp.join("a.bin"), &a).unwrap();
        fs::write(tmp.join("b.bin"), &b).unwrap();

        let result = find_duplicates(&tmp, 1, None).unwrap();
        assert!(result.groups.is_empty());
        // 부분해시 단계까지는 후보로 살아남았어야 한다 (필터가 실제로 동작했는지 확인).
        assert_eq!(result.stats.partial_hashed, 2);

        fs::remove_dir_all(&tmp).unwrap();
    }
}
