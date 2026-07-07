//! 3단 계층 필터 중복 파일 탐지.
//!
//! ① 크기 그룹핑(유일 크기는 즉시 탈락) → ② 헤드+테일 4KiB 부분 해시 →
//! ③ 생존 후보만 blake3 전체 해시. 하드링크는 scanner가 기록 단계에서 이미 1회로
//! 접어두므로 같은 그룹에 중복 등장하지 않는다.
//!
//! F3: APFS clonefile로 이미 블록을 공유하는 쌍은 지워도 공간이 늘지 않는다 —
//! 첫 블록 물리 오프셋(F_LOG2PHYS)으로 감지해 reclaimable을 보정하고,
//! 삭제 대신 무손실 회수(merge_as_clone)를 제공한다.

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
    /// 하나만 남기고 지웠을 때 회수되는 공간 (클론 공유 블록 보정 후).
    pub reclaimable: u64,
    /// true = 그룹 안에 물리 블록을 공유하는(이미 클론인) 파일이 있다.
    pub clone_shared: bool,
    pub hash_hex: String,
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

    // ① 크기 그룹핑.
    let mut by_size: std::collections::HashMap<u64, Vec<FileEntry>> =
        std::collections::HashMap::new();
    for f in files {
        by_size.entry(f.logical_size).or_default().push(f);
    }
    by_size.retain(|_, v| v.len() > 1);
    let candidates: u64 = by_size.values().map(|v| v.len() as u64).sum();

    // ② 같은 크기 그룹 내 부분 해시(헤드+테일 4KiB).
    let partial_groups: Vec<Vec<FileEntry>> = by_size
        .into_par_iter()
        .flat_map(|(size, group)| {
            let mut by_partial: std::collections::HashMap<[u8; 32], Vec<FileEntry>> =
                std::collections::HashMap::new();
            for entry in group {
                if let Ok(h) = partial_hash(&entry.path, size) {
                    if let Some(p) = &progress {
                        p.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    by_partial.entry(h).or_default().push(entry);
                }
            }
            by_partial
                .into_values()
                .filter(|v| v.len() > 1)
                .collect::<Vec<_>>()
        })
        .collect();
    let partial_hashed: u64 = partial_groups.iter().map(|g| g.len() as u64).sum();

    // ③ 생존 후보만 전체 blake3 해시.
    let groups: Vec<DupGroup> = partial_groups
        .into_par_iter()
        .flat_map(|group| {
            let mut by_full: std::collections::HashMap<blake3::Hash, Vec<FileEntry>> =
                std::collections::HashMap::new();
            for entry in group {
                if let Ok(h) = full_hash(&entry.path) {
                    if let Some(p) = &progress {
                        p.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    by_full.entry(h).or_default().push(entry);
                }
            }
            by_full
                .into_iter()
                .filter(|(_, v)| v.len() > 1)
                .map(|(hash, mut v)| {
                    v.sort_by(|a, b| a.path.cmp(&b.path));
                    let size = v[0].logical_size;
                    // 물리 첫 블록이 같은 파일들은 이미 클론 — 지워도 회수 0.
                    // 감지 실패(비-APFS/에러)는 각자 독립 블록으로 간주(기존과 동일).
                    let mut seen_extents = std::collections::HashSet::new();
                    let mut unique = 0u64;
                    for e in &v {
                        match first_block_phys(&e.path) {
                            Some(off) => {
                                if seen_extents.insert(off) {
                                    unique += 1;
                                }
                            }
                            None => unique += 1,
                        }
                    }
                    DupGroup {
                        file_size: size,
                        reclaimable: v[0].allocated_size * unique.saturating_sub(1),
                        clone_shared: unique < v.len() as u64,
                        hash_hex: hash.to_hex().to_string(),
                        files: v.into_iter().map(|e| e.path).collect(),
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let full_hashed: u64 = groups.iter().map(|g| g.files.len() as u64).sum();
    let mut groups = groups;
    groups.sort_by(|a, b| b.reclaimable.cmp(&a.reclaimable));
    Ok(DedupResult {
        groups,
        stats: DedupStats {
            candidates,
            partial_hashed,
            full_hashed,
        },
    })
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
    let mut f = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

// ───────────────────────── APFS 클론 (F3) ─────────────────────────

/// 파일 첫 논리 블록의 물리(디바이스) 오프셋. 같으면 블록 공유(클론/COW)로 판정.
/// 휴리스틱이므로 회수량 *표시* 보정에만 쓰고 삭제 가드에는 쓰지 않는다.
#[cfg(target_os = "macos")]
fn first_block_phys(path: &Path) -> Option<u64> {
    use std::os::fd::AsRawFd;
    // sys/fcntl.h: F_LOG2PHYS = 49, struct log2phys.
    #[repr(C)]
    struct Log2Phys {
        l2p_flags: u32,
        l2p_contigbytes: i64,
        l2p_devoffset: i64,
    }
    const F_LOG2PHYS: libc::c_int = 49;
    let f = File::open(path).ok()?;
    let mut l2p = Log2Phys {
        l2p_flags: 0,
        l2p_contigbytes: 0,
        l2p_devoffset: 0,
    };
    let rc = unsafe { libc::fcntl(f.as_raw_fd(), F_LOG2PHYS, &mut l2p) };
    if rc == -1 || l2p.l2p_devoffset < 0 {
        return None;
    }
    Some(l2p.l2p_devoffset as u64)
}

#[cfg(not(target_os = "macos"))]
fn first_block_phys(_path: &Path) -> Option<u64> {
    None
}

/// merge_group 결과 집계.
#[derive(Debug, Default, Clone, Copy)]
pub struct MergeStats {
    pub merged: u32,
    pub failed: u32,
    /// 회수 추정치 상한 (victim들이 점유하던 블록 합).
    pub reclaimed: u64,
}

/// 그룹 병합: victims를 keep의 클론 사본으로 순차 교체한다.
/// keep은 배치 시작 시 한 번만 해시하고 각 victim만 재해시한다 —
/// N개 victim에 keep을 N번 다시 읽는 낭비를 없앤다.
pub fn merge_group(keep: &Path, victims: &[PathBuf]) -> MergeStats {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = keep;
        MergeStats {
            failed: victims.len() as u32,
            ..Default::default()
        }
    }
    #[cfg(target_os = "macos")]
    {
        let keep_hash = match full_hash(keep) {
            Ok(h) => h,
            Err(_) => {
                return MergeStats {
                    failed: victims.len() as u32,
                    ..Default::default()
                }
            }
        };
        let mut stats = MergeStats::default();
        for victim in victims {
            match merge_one(keep, &keep_hash, victim) {
                Ok(bytes) => {
                    stats.merged += 1;
                    stats.reclaimed += bytes;
                }
                Err(_) => stats.failed += 1,
            }
        }
        stats
    }
}

/// victim을 keep의 APFS 클론 사본으로 교체해 데이터 손실 없이 공간을 회수한다.
/// 반환: 회수 추정치(victim이 점유하던 블록, 상한). 배치는 merge_group 사용.
pub fn merge_as_clone(keep: &Path, victim: &Path) -> std::io::Result<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (keep, victim);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "clonefile requires APFS (macOS)",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        let keep_hash = full_hash(keep)?;
        merge_one(keep, &keep_hash, victim)
    }
}

/// 안전 절차: ① victim을 다시 전체 해시해 keep 해시와 동일성 재확인(TOCTOU 방지)
/// → ② 같은 디렉토리에 임시 이름으로 clonefile → ③ victim의 권한·mtime을 임시
/// 파일에 복사 → ④ rename으로 원자적 교체. 실패 시 어느 단계에서든 victim 무손상.
/// victim에 다른 하드링크가 있으면 거부한다 — rename이 링크 쌍을 조용히 끊는 데다
/// 쌍이 inode를 붙들고 있어 회수도 0이기 때문.
#[cfg(target_os = "macos")]
fn merge_one(keep: &Path, keep_hash: &blake3::Hash, victim: &Path) -> std::io::Result<u64> {
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    extern "C" {
        fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32)
            -> libc::c_int;
    }

    if keep == victim {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "keep and victim are the same path",
        ));
    }
    let victim_md = fs::symlink_metadata(victim)?;
    let keep_md = fs::symlink_metadata(keep)?;
    if !victim_md.is_file() || !keep_md.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "merge targets must be regular files",
        ));
    }
    if victim_md.nlink() > 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "victim has other hardlinks — merging would sever them and reclaim nothing",
        ));
    }
    // ① 내용 동일성 재확인 — 탐지 이후 파일이 바뀌었으면 거부.
    if *keep_hash != full_hash(victim)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "files differ — refusing to merge",
        ));
    }
    // 이미 블록을 공유하면 할 일 없음.
    if let (Some(a), Some(b)) = (first_block_phys(keep), first_block_phys(victim)) {
        if a == b {
            return Ok(0);
        }
    }
    let reclaim = victim_md.blocks() * 512;
    let parent = victim.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "victim has no parent dir")
    })?;
    let tmp = parent.join(format!(".space-mesh-clone-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);

    // ② clonefile — 크로스 볼륨/비-APFS면 여기서 실패한다 (victim 무손상).
    let src = CString::new(keep.as_os_str().as_bytes())?;
    let dst = CString::new(tmp.as_os_str().as_bytes())?;
    if unsafe { clonefile(src.as_ptr(), dst.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    // ③+④ 메타데이터 복사 후 원자적 교체 — 실패 시 임시 파일만 지운다.
    let result = (|| -> std::io::Result<()> {
        fs::set_permissions(&tmp, victim_md.permissions())?;
        let times = [
            libc::timeval {
                tv_sec: victim_md.atime() as libc::time_t,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: victim_md.mtime() as libc::time_t,
                tv_usec: 0,
            },
        ];
        let tmp_c = CString::new(tmp.as_os_str().as_bytes())?;
        if unsafe { libc::utimes(tmp_c.as_ptr(), times.as_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        fs::rename(&tmp, victim)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    Ok(reclaim)
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
    fn merge_as_clone_refuses_or_reclaims() {
        let tmp = std::env::temp_dir().join(format!("space-dedup-clone-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("keep.bin"), vec![7u8; 20_000]).unwrap();
        fs::write(tmp.join("victim.bin"), vec![7u8; 20_000]).unwrap();
        fs::write(tmp.join("other.bin"), vec![8u8; 20_000]).unwrap();

        // 내용이 다른 파일 병합은 어떤 플랫폼에서든 반드시 거부되어야 한다.
        let bad = merge_as_clone(&tmp.join("keep.bin"), &tmp.join("other.bin"));
        assert!(bad.is_err());
        // 거부 후 원본 무손상.
        assert_eq!(fs::read(tmp.join("other.bin")).unwrap(), vec![8u8; 20_000]);

        let same = merge_as_clone(&tmp.join("keep.bin"), &tmp.join("victim.bin"));
        if cfg!(target_os = "macos") {
            // APFS면 성공하고 내용이 보존된다 (tmpdir이 APFS가 아니면 에러 허용).
            if let Ok(reclaimed) = same {
                assert!(reclaimed > 0);
                assert_eq!(fs::read(tmp.join("victim.bin")).unwrap(), vec![7u8; 20_000]);
            }
        } else {
            // 비-macOS는 Unsupported.
            assert_eq!(same.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
        }

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
