//! 증분 병합 엔진 (M4) — 변경 서브트리 재스캔 결과를 기존 트리에 접붙인다.
//!
//! 설계 (research 확정):
//! - 교체 지점은 이름 매칭 (children 순서는 rayon으로 비안정 — index 부적합)
//! - 조상 집계는 델타 가산 O(depth), 서브트리 크기 무관
//! - 대상이 트리에 없으면(신규 중첩) 최근접 존재 조상으로 재스캔 루트 승격
//! - 하드링크 A′: 레지스트리로 (a)이중계산 차단, (b)소유자 소실 + 외부
//!   파트너 가능성은 소유권 이전 없이 풀스캔 강등 (드문 이벤트, 보수적)

use crate::{scan_internal, DirNode, ScanOptions, ScanStats};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// nlink>1 파일의 최초 집계 소유자.
#[derive(Debug, Clone)]
pub struct HardlinkOwner {
    pub path: PathBuf,
    pub logical: u64,
    pub alloc: u64,
    /// 관측 당시 st_nlink — 그룹의 전체 링크 수.
    pub nlink: u64,
    /// 이 스캔 범위 안에서 관측된 횟수. seen == nlink이면 그룹이 스캔 범위에
    /// 자체 완결 — 증분 병합 시 (c)신규 링크 판정에 쓴다.
    pub seen: u64,
}

/// (dev, ino) → 소유자. 풀스캔이 구축하고 증분 병합이 참조·갱신한다.
pub type HardlinkRegistry = HashMap<(u64, u64), HardlinkOwner>;

/// 병합 결과.
#[derive(Debug)]
pub enum MergeVerdict {
    /// 병합 완료 — 실제 재스캔된 서브트리 루트(승격 반영)와 통계.
    Merged {
        rescanned: PathBuf,
        promoted: bool,
        removed: bool,
    },
    /// 증분으로 정확성을 보장할 수 없음 — 호출자가 풀스캔으로 강등할 것.
    Degrade(String),
}

/// target 경로의 서브트리를 재스캔해 root 트리에 병합한다.
///
/// - `root`: 이전 풀스캔의 소유 트리 (호출자가 clone 등으로 확보 — 실측 ~10ms)
/// - `stats`/`registry`: 함께 갱신된다 (Degrade 시 원본 보존 안 함 — 호출자는
///   Degrade를 받으면 병합 중이던 트리/레지스트리를 버리고 풀스캔해야 한다)
pub fn rescan_and_merge(
    root: &mut DirNode,
    root_path: &Path,
    stats: &mut ScanStats,
    registry: &mut HardlinkRegistry,
    target: &Path,
    opts: &ScanOptions,
) -> std::io::Result<MergeVerdict> {
    let Ok(rel) = target.strip_prefix(root_path) else {
        return Ok(MergeVerdict::Degrade(format!(
            "target {} is outside root",
            target.display()
        )));
    };
    let comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if comps.is_empty() {
        return Ok(MergeVerdict::Degrade("target == root (full rescan)".into()));
    }

    // 트리에 존재하는 최심 프리픽스 길이 k를 찾는다. k < len이면 승격.
    let k = existing_prefix_len(root, &comps);
    if k == 0 {
        // 루트 직속 자식조차 없음(신규 최상위) — 루트 재스캔 = 풀스캔.
        return Ok(MergeVerdict::Degrade(
            "nearest existing ancestor is root".into(),
        ));
    }
    let promoted = k < comps.len();
    let node_comps = &comps[..k];
    let node_path = root_path.join(node_comps.join("/"));

    // 이전 서브트리 집계 (델타 계산용).
    let old = find_node(root, node_comps)
        .map(|n| (n.logical_size, n.allocated_size, n.file_count, n.dir_count));
    let Some((old_l, old_a, old_f, old_d)) = old else {
        return Ok(MergeVerdict::Degrade("tree lookup failed".into()));
    };

    // (b) 강등 검사 준비: 소유자가 이 서브트리 안에 있는 레지스트리 항목들.
    let owned_inside: Vec<(u64, u64)> = registry
        .iter()
        .filter(|(_, o)| o.path.starts_with(&node_path))
        .map(|(k, _)| *k)
        .collect();

    if !fs_is_dir(&node_path) {
        // 서브트리가 디스크에서 사라짐 — 제거 병합.
        if !owned_inside.is_empty() {
            // 소유자가 사라졌고 파트너가 밖에 있을 수 있음 — A′: 강등.
            return Ok(MergeVerdict::Degrade(format!(
                "removed subtree owned {} hardlink group(s)",
                owned_inside.len()
            )));
        }
        remove_child(root, node_comps);
        apply_ancestor_delta(
            root,
            &node_comps[..node_comps.len() - 1],
            -(old_l as i128),
            -(old_a as i128),
            -(old_f as i128),
            -(old_d as i128),
        );
        refresh_stats(root, stats);
        return Ok(MergeVerdict::Merged {
            rescanned: node_path,
            promoted,
            removed: true,
        });
    }

    // 서브트리 재스캔 — 밖에서 이미 집계된 (dev,ino)는 preseen으로 차단.
    let preseen: HashSet<(u64, u64)> = registry
        .iter()
        .filter(|(_, o)| !o.path.starts_with(&node_path))
        .map(|(k, _)| *k)
        .collect();
    let sub = scan_internal(&node_path, opts.clone(), None, Some(Arc::new(preseen)))?;

    // (b) 강등 검사: 안에 있던 소유자가 재스캔에서 다시 보이지 않으면,
    // 밖의 파트너가 미집계 상태로 남을 수 있다 — 소유권 이전 대신 강등.
    for key in &owned_inside {
        if !sub.hardlinks.contains_key(key) {
            return Ok(MergeVerdict::Degrade(format!(
                "hardlink owner vanished from rescanned subtree (dev,ino)={:?}",
                key
            )));
        }
    }

    // (c) 강등 검사: 레지스트리에 없는 신규 하드링크 그룹이 서브트리 안에서
    // 자체 완결(seen == nlink)이 아니면, 밖의 파트너가 일반 파일로 이미
    // 집계돼 있었을 수 있다(nlink 1→2 승격) — 이중 계산 위험이므로 강등.
    for (key, owner) in &sub.hardlinks {
        if !registry.contains_key(key) && owner.seen < owner.nlink {
            return Ok(MergeVerdict::Degrade(format!(
                "new hardlink group with partner outside subtree (dev,ino)={:?}",
                key
            )));
        }
    }

    // (d) 강등 검사: preseen으로 스킵한 링크의 관측 크기가 레지스트리 기록과
    // 다르면, 비소유자 경로를 통한 내용 수정 — 소유자 쪽 집계가 낡았다.
    for (key, (l, a)) in &sub.preseen_hits {
        if let Some(o) = registry.get(key) {
            if o.logical != *l || o.alloc != *a {
                return Ok(MergeVerdict::Degrade(format!(
                    "hardlink modified via non-owner path (dev,ino)={:?}",
                    key
                )));
            }
        }
    }

    // 레지스트리 갱신: 서브트리 내 소유자들을 새 관측으로 교체/추가.
    for (key, owner) in &sub.hardlinks {
        registry.insert(*key, owner.clone());
    }

    // 접붙이기 + 조상 델타.
    let new = &sub.root;
    let (dl, da, df, dd) = (
        new.logical_size as i128 - old_l as i128,
        new.allocated_size as i128 - old_a as i128,
        new.file_count as i128 - old_f as i128,
        new.dir_count as i128 - old_d as i128,
    );
    replace_child(root, node_comps, sub.root);
    apply_ancestor_delta(root, &node_comps[..node_comps.len() - 1], dl, da, df, dd);
    stats.errors += sub.stats.errors;
    refresh_stats(root, stats);

    Ok(MergeVerdict::Merged {
        rescanned: node_path,
        promoted,
        removed: false,
    })
}

fn fs_is_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|m| m.is_dir())
        .unwrap_or(false)
}

/// comps 중 트리에 이름 매칭으로 존재하는 최심 프리픽스 길이.
fn existing_prefix_len(root: &DirNode, comps: &[String]) -> usize {
    let mut node = root;
    for (i, c) in comps.iter().enumerate() {
        match node.children.iter().find(|ch| &ch.name == c) {
            Some(ch) => node = ch,
            None => return i,
        }
    }
    comps.len()
}

fn find_node<'a>(root: &'a DirNode, comps: &[String]) -> Option<&'a DirNode> {
    let mut node = root;
    for c in comps {
        node = node.children.iter().find(|ch| &ch.name == c)?;
    }
    Some(node)
}

fn find_node_mut<'a>(root: &'a mut DirNode, comps: &[String]) -> Option<&'a mut DirNode> {
    let mut node = root;
    for c in comps {
        node = node.children.iter_mut().find(|ch| &ch.name == c)?;
    }
    Some(node)
}

/// comps가 가리키는 자식을 새 서브트리로 교체한다.
fn replace_child(root: &mut DirNode, comps: &[String], new_node: DirNode) {
    let (parent_comps, name) = comps.split_at(comps.len() - 1);
    if let Some(parent) = find_node_mut(root, parent_comps) {
        if let Some(slot) = parent.children.iter_mut().find(|ch| ch.name == name[0]) {
            *slot = new_node;
        } else {
            parent.children.push(new_node);
        }
    }
}

fn remove_child(root: &mut DirNode, comps: &[String]) {
    let (parent_comps, name) = comps.split_at(comps.len() - 1);
    if let Some(parent) = find_node_mut(root, parent_comps) {
        parent.children.retain(|ch| ch.name != name[0]);
    }
}

/// 루트부터 parent_comps 경로상의 모든 조상(루트 포함)에 델타를 가산한다.
fn apply_ancestor_delta(
    root: &mut DirNode,
    parent_comps: &[String],
    dl: i128,
    da: i128,
    df: i128,
    dd: i128,
) {
    let add = |node: &mut DirNode| {
        node.logical_size = (node.logical_size as i128 + dl).max(0) as u64;
        node.allocated_size = (node.allocated_size as i128 + da).max(0) as u64;
        node.file_count = (node.file_count as i128 + df).max(0) as u64;
        node.dir_count = (node.dir_count as i128 + dd).max(0) as u64;
    };
    let mut node = root;
    add(node);
    for c in parent_comps {
        match node.children.iter_mut().find(|ch| &ch.name == c) {
            Some(ch) => {
                node = ch;
                add(node);
            }
            None => return,
        }
    }
}

/// 병합 후 전체 통계를 루트 집계로부터 재유도한다.
fn refresh_stats(root: &DirNode, stats: &mut ScanStats) {
    stats.total_files = root.file_count;
    stats.total_dirs = root.dir_count;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{scan, ScanOptions};
    use std::fs;

    fn opts() -> ScanOptions {
        ScanOptions {
            record_file_threshold: 1000,
            ..Default::default()
        }
    }

    /// 두 트리의 집계·구조·big_files 동등성 (이름 기준 재귀, 순서 무관).
    fn assert_tree_eq(a: &DirNode, b: &DirNode, path: &str) {
        assert_eq!(a.logical_size, b.logical_size, "logical @ {path}");
        assert_eq!(a.allocated_size, b.allocated_size, "allocated @ {path}");
        assert_eq!(a.file_count, b.file_count, "file_count @ {path}");
        assert_eq!(a.dir_count, b.dir_count, "dir_count @ {path}");
        let mut bf_a: Vec<_> = a.big_files.iter().map(|f| (&f.path, f.allocated_size)).collect();
        let mut bf_b: Vec<_> = b.big_files.iter().map(|f| (&f.path, f.allocated_size)).collect();
        bf_a.sort();
        bf_b.sort();
        assert_eq!(bf_a, bf_b, "big_files @ {path}");
        assert_eq!(a.children.len(), b.children.len(), "children count @ {path}");
        for ca in &a.children {
            let cb = b
                .children
                .iter()
                .find(|c| c.name == ca.name)
                .unwrap_or_else(|| panic!("child {} missing @ {path}", ca.name));
            assert_tree_eq(ca, cb, &format!("{path}/{}", ca.name));
        }
    }

    /// 증분 병합 결과가 풀스캔과 동등해야 한다 — 파일 추가/수정/삭제/디렉토리.
    #[test]
    fn merge_matches_full_scan_for_basic_mutations() {
        let tmp = std::env::temp_dir().join(format!("space-merge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("a/deep")).unwrap();
        fs::create_dir_all(tmp.join("b")).unwrap();
        fs::write(tmp.join("a/one.bin"), vec![1u8; 4000]).unwrap();
        fs::write(tmp.join("a/deep/two.bin"), vec![2u8; 6000]).unwrap();
        fs::write(tmp.join("b/three.bin"), vec![3u8; 2000]).unwrap();

        let base = scan(&tmp, opts()).unwrap();
        let mut root = base.root.clone();
        let mut stats = ScanStats {
            errors: base.stats.errors,
            total_files: base.stats.total_files,
            total_dirs: base.stats.total_dirs,
        };
        let mut registry = base.hardlinks.clone();

        // 변경: a/deep에 파일 추가 + two.bin 크기 변경, b/three.bin 삭제, 새 디렉토리 b/nested/new.
        fs::write(tmp.join("a/deep/added.bin"), vec![4u8; 8000]).unwrap();
        fs::write(tmp.join("a/deep/two.bin"), vec![2u8; 12000]).unwrap();
        fs::remove_file(tmp.join("b/three.bin")).unwrap();
        fs::create_dir_all(tmp.join("b/nested/new")).unwrap();
        fs::write(tmp.join("b/nested/new/four.bin"), vec![5u8; 3000]).unwrap();

        // FSEvents가 줄 법한 이벤트: a/deep 변경, b 변경(삭제+nested 생성).
        for target in [tmp.join("a/deep"), tmp.join("b")] {
            let v = rescan_and_merge(&mut root, &tmp, &mut stats, &mut registry, &target, &opts())
                .unwrap();
            assert!(matches!(v, MergeVerdict::Merged { .. }), "{v:?}");
        }

        let full = scan(&tmp, opts()).unwrap();
        assert_tree_eq(&root, &full.root, "root");
        assert_eq!(stats.total_files, full.stats.total_files);
        assert_eq!(stats.total_dirs, full.stats.total_dirs);

        fs::remove_dir_all(&tmp).unwrap();
    }

    /// 신규 중첩 체인 — 트리에 없는 깊은 경로 이벤트는 조상으로 승격.
    #[test]
    fn nested_new_chain_promotes_to_existing_ancestor() {
        let tmp = std::env::temp_dir().join(format!("space-merge-nest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("proj")).unwrap();
        fs::write(tmp.join("proj/base.bin"), vec![1u8; 2000]).unwrap();

        let base = scan(&tmp, opts()).unwrap();
        let mut root = base.root.clone();
        let mut stats = base.stats;
        let mut registry = base.hardlinks.clone();

        fs::create_dir_all(tmp.join("proj/x/y/z")).unwrap();
        fs::write(tmp.join("proj/x/y/z/deep.bin"), vec![2u8; 5000]).unwrap();

        // 트리에 없는 깊은 경로 이벤트 → proj로 승격되어야 함.
        let v = rescan_and_merge(
            &mut root,
            &tmp,
            &mut stats,
            &mut registry,
            &tmp.join("proj/x/y/z"),
            &opts(),
        )
        .unwrap();
        match v {
            MergeVerdict::Merged {
                promoted,
                rescanned,
                ..
            } => {
                assert!(promoted);
                assert!(rescanned.ends_with("proj"), "{rescanned:?}");
            }
            other => panic!("{other:?}"),
        }
        let full = scan(&tmp, opts()).unwrap();
        assert_tree_eq(&root, &full.root, "root");

        fs::remove_dir_all(&tmp).unwrap();
    }

    /// 하드링크 (a): 파트너가 서브트리 밖 — 재스캔이 이중 계산하지 않아야 한다.
    #[test]
    fn hardlink_outside_partner_not_double_counted() {
        let tmp = std::env::temp_dir().join(format!("space-merge-hla-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("a")).unwrap();
        fs::create_dir_all(tmp.join("b")).unwrap();
        fs::write(tmp.join("a/orig.bin"), vec![7u8; 9000]).unwrap();
        fs::hard_link(tmp.join("a/orig.bin"), tmp.join("b/link.bin")).unwrap();

        let base = scan(&tmp, opts()).unwrap();
        assert_eq!(base.hardlinks.len(), 1);
        let mut root = base.root.clone();
        let mut stats = base.stats;
        let mut registry = base.hardlinks.clone();

        // b만 변경(무관 파일 추가) — b 재스캔이 link.bin을 다시 세면 안 된다
        // (소유자가 a/orig.bin인 경우; 소유자가 b쪽이면 (b)강등 경로로 빠짐 —
        // 둘 다 "풀스캔과 동등"이라는 결과 계약은 동일하다).
        fs::write(tmp.join("b/extra.bin"), vec![8u8; 1500]).unwrap();
        let v = rescan_and_merge(
            &mut root,
            &tmp,
            &mut stats,
            &mut registry,
            &tmp.join("b"),
            &opts(),
        )
        .unwrap();

        let full = scan(&tmp, opts()).unwrap();
        match v {
            MergeVerdict::Merged { .. } => {
                // 교차 디렉토리 하드링크는 디렉토리별 귀속이 비결정적 — 총량으로 검증.
                assert_eq!(root.logical_size, full.root.logical_size);
                assert_eq!(root.allocated_size, full.root.allocated_size);
                assert_eq!(root.file_count, full.root.file_count);
                assert_eq!(stats.total_files, full.stats.total_files);
            }
            MergeVerdict::Degrade(_) => {
                // 소유자가 b쪽이었던 경우 — 강등이 계약대로 동작 (호출자가 풀스캔).
            }
        }

        fs::remove_dir_all(&tmp).unwrap();
    }

    /// property 하네스: 시드 고정 랜덤 mutation 시퀀스 — 매 스텝 증분 == 풀스캔.
    /// Degrade가 나오면 호출자 계약대로 풀스캔 재베이스 후 계속한다.
    #[test]
    fn property_random_mutations_match_full_scan() {
        let tmp = std::env::temp_dir().join(format!("space-merge-prop-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        for d in ["a", "a/x", "b", "b/y", "c"] {
            fs::create_dir_all(tmp.join(d)).unwrap();
        }

        // 결정적 LCG (외부 의존성 없이 재현 가능).
        let mut seed: u64 = 0xDEAD_BEEF_1234_5678;
        let mut rng = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };

        let dirs = ["a", "a/x", "b", "b/y", "c"];
        let mut files: Vec<PathBuf> = Vec::new();
        let mut degrades = 0u32;

        let base = scan(&tmp, opts()).unwrap();
        let mut root = base.root.clone();
        let mut stats = base.stats;
        let mut registry = base.hardlinks.clone();

        for step in 0..80 {
            // mutation 선택: 생성/수정/삭제/하드링크/디렉토리.
            let mut touched: Vec<PathBuf> = Vec::new();
            match rng() % 6 {
                0 | 1 => {
                    // 파일 생성
                    let d = dirs[rng() % dirs.len()];
                    let p = tmp.join(d).join(format!("f{step}.bin"));
                    fs::write(&p, vec![7u8; 500 + rng() % 5000]).unwrap();
                    touched.push(tmp.join(d));
                    files.push(p);
                }
                2 => {
                    // 파일 크기 변경
                    if let Some(p) = files.get(rng() % files.len().max(1)).cloned() {
                        if p.exists() {
                            fs::write(&p, vec![8u8; 500 + rng() % 8000]).unwrap();
                            touched.push(p.parent().unwrap().to_path_buf());
                        }
                    }
                }
                3 => {
                    // 파일 삭제
                    if !files.is_empty() {
                        let p = files.swap_remove(rng() % files.len());
                        if p.exists() {
                            let _ = fs::remove_file(&p);
                            touched.push(p.parent().unwrap().to_path_buf());
                        }
                    }
                }
                4 => {
                    // 하드링크 생성 (다른 디렉토리로)
                    if let Some(src) = files.get(rng() % files.len().max(1)).cloned() {
                        if src.exists() {
                            let d = dirs[rng() % dirs.len()];
                            let dst = tmp.join(d).join(format!("hl{step}.bin"));
                            if fs::hard_link(&src, &dst).is_ok() {
                                touched.push(tmp.join(d));
                                files.push(dst);
                            }
                        }
                    }
                }
                _ => {
                    // 중첩 디렉토리 생성 + 파일
                    let d = dirs[rng() % dirs.len()];
                    let nd = tmp.join(d).join(format!("n{step}/deep"));
                    fs::create_dir_all(&nd).unwrap();
                    fs::write(nd.join("g.bin"), vec![9u8; 1000 + rng() % 3000]).unwrap();
                    touched.push(tmp.join(d).join(format!("n{step}/deep")));
                }
            }

            // 증분 병합 (Degrade 시 호출자 계약: 풀스캔 재베이스).
            for t in &touched {
                let v = rescan_and_merge(&mut root, &tmp, &mut stats, &mut registry, t, &opts())
                    .unwrap();
                if let MergeVerdict::Degrade(_) = v {
                    degrades += 1;
                    let fresh = scan(&tmp, opts()).unwrap();
                    root = fresh.root;
                    stats = fresh.stats;
                    registry = fresh.hardlinks;
                    break;
                }
            }

            // 불변식: 매 스텝 루트 총량 == 풀스캔.
            // 주의: 교차 디렉토리 하드링크 그룹의 디렉토리별 귀속은 풀스캔끼리도
            // rayon 순서에 따라 비결정적(기존 동작) — 제품 보장은 총량 동등이다.
            let full = scan(&tmp, opts()).unwrap();
            assert_eq!(root.logical_size, full.root.logical_size, "logical step {step}");
            assert_eq!(root.allocated_size, full.root.allocated_size, "alloc step {step}");
            assert_eq!(root.file_count, full.root.file_count, "files step {step}");
            assert_eq!(root.dir_count, full.root.dir_count, "dirs step {step}");
            assert_eq!(stats.total_files, full.stats.total_files, "step {step}");
            assert_eq!(stats.total_dirs, full.stats.total_dirs, "step {step}");
        }
        // 하드링크 삭제가 섞이면 강등이 발생할 수 있음 — 발생 여부만 기록.
        eprintln!("property: 80 steps, degrades={degrades}");

        fs::remove_dir_all(&tmp).unwrap();
    }

    /// 하드링크 (b): 재스캔 서브트리 안의 소유자가 삭제되고 파트너가 밖에 있음 → 강등.
    #[test]
    fn hardlink_owner_deletion_degrades() {
        let tmp = std::env::temp_dir().join(format!("space-merge-hlb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("a")).unwrap();
        fs::create_dir_all(tmp.join("b")).unwrap();
        fs::write(tmp.join("a/orig.bin"), vec![7u8; 9000]).unwrap();
        fs::hard_link(tmp.join("a/orig.bin"), tmp.join("b/link.bin")).unwrap();

        let base = scan(&tmp, opts()).unwrap();
        let owner_in_a = base.hardlinks.values().next().unwrap().path.starts_with(tmp.join("a"));
        let mut root = base.root.clone();
        let mut stats = base.stats;
        let mut registry = base.hardlinks.clone();

        // 소유자 쪽 파일을 삭제하고 그 서브트리를 재스캔하면 강등이어야 한다.
        let (owner_dir, victim) = if owner_in_a {
            (tmp.join("a"), tmp.join("a/orig.bin"))
        } else {
            (tmp.join("b"), tmp.join("b/link.bin"))
        };
        fs::remove_file(&victim).unwrap();
        let v = rescan_and_merge(
            &mut root,
            &tmp,
            &mut stats,
            &mut registry,
            &owner_dir,
            &opts(),
        )
        .unwrap();
        assert!(matches!(v, MergeVerdict::Degrade(_)), "{v:?}");

        fs::remove_dir_all(&tmp).unwrap();
    }
}
