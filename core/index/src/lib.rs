//! 스캔 결과의 SQLite 스냅샷 캐시.
//!
//! 스캔 트리(디렉토리 집계 + 대용량 파일)를 저장해 앱/CLI 재실행 시
//! 재스캔 없이 이전 상태를 즉시 로드한다. FSEvents 증분 반영은 M4에서 이 위에 얹는다.

use rusqlite::{params, Connection};
use space_scanner::{DirNode, FileEntry, ScanResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct SnapshotMeta {
    pub scan_id: i64,
    pub root_path: String,
    pub created_at: String,
    pub total_files: u64,
    pub total_dirs: u64,
}

/// 루트당 유지할 기본 스냅샷 개수 (PERF-005 — 무한 축적 방지).
pub const DEFAULT_KEEP_SNAPSHOTS: usize = 30;

pub fn open(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // SQLite는 연결별로 foreign_keys가 기본 OFF — 켜지 않으면 스키마의
    // ON DELETE CASCADE가 동작하지 않는다.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS scans (
            id INTEGER PRIMARY KEY,
            root_path TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
            total_files INTEGER NOT NULL,
            total_dirs INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS nodes (
            scan_id INTEGER NOT NULL REFERENCES scans(id) ON DELETE CASCADE,
            node_id INTEGER NOT NULL,
            parent_id INTEGER,
            name TEXT NOT NULL,
            logical_size INTEGER NOT NULL,
            allocated_size INTEGER NOT NULL,
            file_count INTEGER NOT NULL,
            dir_count INTEGER NOT NULL,
            PRIMARY KEY (scan_id, node_id)
        );
        CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes(scan_id, parent_id);
        CREATE TABLE IF NOT EXISTS big_files (
            scan_id INTEGER NOT NULL REFERENCES scans(id) ON DELETE CASCADE,
            node_id INTEGER NOT NULL,
            path TEXT NOT NULL,
            logical_size INTEGER NOT NULL,
            allocated_size INTEGER NOT NULL,
            modified_epoch INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_big_files_scan ON big_files(scan_id);",
    )?;
    // 구버전 DB 마이그레이션: modified_epoch 컬럼 추가 (이미 있으면 에러 무시).
    let _ = conn.execute(
        "ALTER TABLE big_files ADD COLUMN modified_epoch INTEGER NOT NULL DEFAULT 0",
        [],
    );
    Ok(conn)
}

/// 스캔 결과 전체를 하나의 트랜잭션으로 저장하고 scan_id를 반환한다.
pub fn save_snapshot(
    conn: &mut Connection,
    root_path: &Path,
    result: &ScanResult,
) -> rusqlite::Result<i64> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO scans (root_path, total_files, total_dirs) VALUES (?1, ?2, ?3)",
        params![
            root_path.to_string_lossy(),
            result.stats.total_files as i64,
            result.stats.total_dirs as i64
        ],
    )?;
    let scan_id = tx.last_insert_rowid();
    {
        let mut node_stmt = tx.prepare(
            "INSERT INTO nodes (scan_id, node_id, parent_id, name, logical_size, allocated_size, file_count, dir_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        let mut file_stmt = tx.prepare(
            "INSERT INTO big_files (scan_id, node_id, path, logical_size, allocated_size, modified_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;

        // 명시적 스택으로 순회 (트리 깊이에 따른 콜스택 부담 회피).
        let mut next_id: i64 = 0;
        let mut stack: Vec<(&DirNode, Option<i64>)> = vec![(&result.root, None)];
        while let Some((node, parent_id)) = stack.pop() {
            let node_id = next_id;
            next_id += 1;
            node_stmt.execute(params![
                scan_id,
                node_id,
                parent_id,
                node.name,
                node.logical_size as i64,
                node.allocated_size as i64,
                node.file_count as i64,
                node.dir_count as i64
            ])?;
            for f in &node.big_files {
                file_stmt.execute(params![
                    scan_id,
                    node_id,
                    f.path.to_string_lossy(),
                    f.logical_size as i64,
                    f.allocated_size as i64,
                    f.modified_epoch
                ])?;
            }
            for c in &node.children {
                stack.push((c, Some(node_id)));
            }
        }
    }
    tx.commit()?;
    Ok(scan_id)
}

/// 해당 루트 경로의 가장 최근 스냅샷을 로드한다.
pub fn load_latest(
    conn: &Connection,
    root_path: &Path,
) -> rusqlite::Result<Option<(SnapshotMeta, DirNode)>> {
    let meta: Option<SnapshotMeta> = conn
        .query_row(
            "SELECT id, root_path, created_at, total_files, total_dirs
             FROM scans WHERE root_path = ?1 ORDER BY id DESC LIMIT 1",
            params![root_path.to_string_lossy()],
            |row| {
                Ok(SnapshotMeta {
                    scan_id: row.get(0)?,
                    root_path: row.get(1)?,
                    created_at: row.get(2)?,
                    total_files: row.get::<_, i64>(3)? as u64,
                    total_dirs: row.get::<_, i64>(4)? as u64,
                })
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    let Some(meta) = meta else { return Ok(None) };
    match load_tree(conn, meta.scan_id)? {
        Some(root) => Ok(Some((meta, root))),
        None => Ok(None),
    }
}

/// scan_id의 노드 전체를 읽어 parent 관계로 트리를 재조립한다.
fn load_tree(conn: &Connection, scan_id: i64) -> rusqlite::Result<Option<DirNode>> {
    struct Row {
        node_id: i64,
        parent_id: Option<i64>,
        node: DirNode,
    }
    let mut stmt = conn.prepare(
        "SELECT node_id, parent_id, name, logical_size, allocated_size, file_count, dir_count
         FROM nodes WHERE scan_id = ?1",
    )?;
    let rows: Vec<Row> = stmt
        .query_map(params![scan_id], |row| {
            Ok(Row {
                node_id: row.get(0)?,
                parent_id: row.get(1)?,
                node: DirNode {
                    name: row.get(2)?,
                    logical_size: row.get::<_, i64>(3)? as u64,
                    allocated_size: row.get::<_, i64>(4)? as u64,
                    file_count: row.get::<_, i64>(5)? as u64,
                    dir_count: row.get::<_, i64>(6)? as u64,
                    children: Vec::new(),
                    big_files: Vec::new(),
                },
            })
        })?
        .collect::<Result<_, _>>()?;

    let mut big: HashMap<i64, Vec<FileEntry>> = HashMap::new();
    let mut fstmt = conn.prepare(
        "SELECT node_id, path, logical_size, allocated_size, modified_epoch
         FROM big_files WHERE scan_id = ?1",
    )?;
    let files = fstmt.query_map(params![scan_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            FileEntry {
                path: PathBuf::from(row.get::<_, String>(1)?),
                logical_size: row.get::<_, i64>(2)? as u64,
                allocated_size: row.get::<_, i64>(3)? as u64,
                modified_epoch: row.get::<_, i64>(4)?,
            },
        ))
    })?;
    for f in files {
        let (node_id, entry) = f?;
        big.entry(node_id).or_default().push(entry);
    }

    // 재귀 없이 바닥부터 조립한다 (PERF-006 — 깊은 트리 콜스택 위험 제거).
    // save_snapshot이 스택 순회로 부모에게 항상 자식보다 작은 node_id를
    // 부여하므로, node_id 내림차순 처리 = 자식이 항상 부모보다 먼저 완성된다.
    let mut children_ids: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut nodes: HashMap<i64, (Option<i64>, DirNode)> = HashMap::new();
    for r in rows {
        if let Some(p) = r.parent_id {
            children_ids.entry(p).or_default().push(r.node_id);
        }
        nodes.insert(r.node_id, (r.parent_id, r.node));
    }
    let mut ids: Vec<i64> = nodes.keys().copied().collect();
    ids.sort_unstable_by_key(|id| std::cmp::Reverse(*id));
    for id in ids {
        if let Some(files) = big.remove(&id) {
            if let Some((_, node)) = nodes.get_mut(&id) {
                node.big_files = files;
            }
        }
        if let Some(kid_ids) = children_ids.remove(&id) {
            // 자식들은 이미(더 큰 id) 서브트리까지 완성된 상태 — 부모로 이동.
            let kids: Vec<DirNode> = kid_ids
                .into_iter()
                .filter_map(|k| nodes.remove(&k).map(|(_, n)| n))
                .collect();
            if let Some((_, node)) = nodes.get_mut(&id) {
                node.children = kids;
            }
        }
    }
    // 남는 것은 루트(parent NULL)뿐이어야 한다.
    Ok(nodes
        .into_values()
        .find(|(parent, _)| parent.is_none())
        .map(|(_, node)| node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use space_scanner::{scan, ScanOptions};
    use std::fs;

    #[test]
    fn roundtrip_snapshot() {
        let tmp = std::env::temp_dir().join(format!("space-index-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("sub")).unwrap();
        fs::write(tmp.join("a.bin"), vec![1u8; 5000]).unwrap();
        fs::write(tmp.join("sub/b.bin"), vec![2u8; 7000]).unwrap();

        let result = scan(
            &tmp,
            ScanOptions {
                record_file_threshold: 1000,
                ..Default::default()
            },
        )
        .unwrap();

        let db = tmp.join("snap.db");
        let mut conn = open(&db).unwrap();
        let scan_id = save_snapshot(&mut conn, &tmp, &result).unwrap();
        assert!(scan_id > 0);

        let (meta, loaded) = load_latest(&conn, &tmp).unwrap().unwrap();
        assert_eq!(meta.total_files, result.stats.total_files);
        assert_eq!(loaded.logical_size, result.root.logical_size);
        assert_eq!(loaded.allocated_size, result.root.allocated_size);
        assert_eq!(loaded.dir_count, result.root.dir_count);
        // 자식과 big_files까지 복원되는지 확인.
        assert_eq!(loaded.children.len(), result.root.children.len());
        let total_big: usize = {
            fn count(n: &DirNode) -> usize {
                n.big_files.len() + n.children.iter().map(count).sum::<usize>()
            }
            count(&loaded)
        };
        assert_eq!(total_big, 2);
        // mtime도 저장/복원되어야 한다 (방금 만든 파일이므로 0일 수 없음).
        {
            fn assert_mtimes(n: &DirNode) {
                for f in &n.big_files {
                    assert!(f.modified_epoch > 0, "mtime lost for {:?}", f.path);
                }
                n.children.iter().for_each(assert_mtimes);
            }
            assert_mtimes(&loaded);
        }

        drop(conn);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prune_keeps_recent_and_removes_orphans() {
        let tmp = std::env::temp_dir().join(format!("space-index-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("a.bin"), vec![1u8; 5000]).unwrap();

        let db = tmp.join("snap.db");
        let mut conn = open(&db).unwrap();
        let result = scan(
            &tmp,
            ScanOptions {
                record_file_threshold: 1000,
                ..Default::default()
            },
        )
        .unwrap();
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(save_snapshot(&mut conn, &tmp, &result).unwrap());
        }

        let pruned = prune_snapshots(&mut conn, &tmp, 2).unwrap();
        assert_eq!(pruned, 1);
        let remaining = list_snapshots(&conn, &tmp).unwrap();
        assert_eq!(remaining.len(), 2);
        // 최신 2개가 남고, 가장 오래된 스냅샷의 노드/파일 행도 함께 사라져야 한다.
        assert!(remaining.iter().all(|m| m.scan_id != ids[0]));
        let orphan_nodes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE scan_id = ?1",
                params![ids[0]],
                |r| r.get(0),
            )
            .unwrap();
        let orphan_files: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM big_files WHERE scan_id = ?1",
                params![ids[0]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!((orphan_nodes, orphan_files), (0, 0));

        drop(conn);
        let _ = fs::remove_dir_all(&tmp);
    }
}

// ───────────────────────── 스냅샷 diff (시계열 분석) ─────────────────────────

/// 해당 루트의 스냅샷 목록 (최신순).
pub fn list_snapshots(conn: &Connection, root_path: &Path) -> rusqlite::Result<Vec<SnapshotMeta>> {
    let mut stmt = conn.prepare(
        "SELECT id, root_path, created_at, total_files, total_dirs
         FROM scans WHERE root_path = ?1 ORDER BY id DESC",
    )?;
    let rows = stmt.query_map(params![root_path.to_string_lossy()], |row| {
        Ok(SnapshotMeta {
            scan_id: row.get(0)?,
            root_path: row.get(1)?,
            created_at: row.get(2)?,
            total_files: row.get::<_, i64>(3)? as u64,
            total_dirs: row.get::<_, i64>(4)? as u64,
        })
    })?;
    rows.collect()
}

/// scan_id로 스냅샷 트리 로드.
pub fn load_by_id(conn: &Connection, scan_id: i64) -> rusqlite::Result<Option<DirNode>> {
    load_tree(conn, scan_id)
}

/// 루트당 최근 keep개만 남기고 오래된 스냅샷을 삭제한다 (PERF-005).
/// 구버전 DB는 연결 당시 foreign_keys가 꺼진 채 쌓였을 수 있으므로
/// CASCADE에 기대지 않고 명시적으로 지운다. 반환: 삭제한 스냅샷 수.
pub fn prune_snapshots(
    conn: &mut Connection,
    root_path: &Path,
    keep: usize,
) -> rusqlite::Result<u64> {
    let tx = conn.transaction()?;
    let old_ids: Vec<i64> = {
        let mut stmt = tx.prepare(
            "SELECT id FROM scans WHERE root_path = ?1 ORDER BY id DESC LIMIT -1 OFFSET ?2",
        )?;
        let rows = stmt.query_map(params![root_path.to_string_lossy(), keep as i64], |r| {
            r.get(0)
        })?;
        rows.collect::<Result<_, _>>()?
    };
    for id in &old_ids {
        tx.execute("DELETE FROM big_files WHERE scan_id = ?1", params![id])?;
        tx.execute("DELETE FROM nodes WHERE scan_id = ?1", params![id])?;
        tx.execute("DELETE FROM scans WHERE id = ?1", params![id])?;
    }
    tx.commit()?;
    Ok(old_ids.len() as u64)
}

/// 두 스냅샷 간 변화의 "범인"을 찾는다 — 잔차 귀속(residual attribution):
/// 각 디렉토리에서 유의미한(≥ min_delta) 하위 변화는 재귀로 내려보내고,
/// 하위로 설명되지 않는 잔차(직속 파일 변화 + 미세 변화 합)가 유의미하면 그 디렉토리를 보고한다.
/// 보고 항목들은 서로 겹치지 않으며 합계가 전체 delta에 근사한다.
#[derive(Debug, Clone)]
pub struct DiffEntry {
    /// 루트 이름부터의 상대 경로.
    pub path: String,
    /// 이 항목에 귀속된 변화량 (bytes, 음수 = 감소).
    pub delta: i64,
    /// 해당 디렉토리의 전/후 총 allocated (컨텍스트용).
    pub before_total: u64,
    pub after_total: u64,
    /// true면 delta가 서브트리 전체가 아니라 "하위로 설명되지 않는 잔차"라는 뜻.
    pub is_residual: bool,
}

pub fn diff_snapshots(
    conn: &Connection,
    old_id: i64,
    new_id: i64,
    min_delta: u64,
) -> rusqlite::Result<Vec<DiffEntry>> {
    let old = load_by_id(conn, old_id)?;
    let new = load_by_id(conn, new_id)?;
    Ok(diff_trees(old.as_ref(), new.as_ref(), min_delta))
}

/// 이미 로드된 두 트리의 diff — 반복 조회(drilldown) 시 트리를 상주시켜 재사용한다.
pub fn diff_trees(old: Option<&DirNode>, new: Option<&DirNode>, min_delta: u64) -> Vec<DiffEntry> {
    let mut out = Vec::new();
    let root_name = new.or(old).map(|n| n.name.clone()).unwrap_or_default();
    attribute(old, new, &root_name, min_delta as i64, &mut out);
    out.sort_by_key(|f| std::cmp::Reverse(f.delta.abs()));
    out
}

fn alloc_of(node: Option<&DirNode>) -> i64 {
    node.map(|n| n.allocated_size as i64).unwrap_or(0)
}

fn attribute(
    old: Option<&DirNode>,
    new: Option<&DirNode>,
    path: &str,
    min_delta: i64,
    out: &mut Vec<DiffEntry>,
) {
    let before = alloc_of(old);
    let after = alloc_of(new);
    let delta = after - before;
    // 이 서브트리에서 min_delta 이상의 변화가 나올 수 없으면 중단.
    if before.max(after) < min_delta && delta.abs() < min_delta {
        return;
    }

    use std::collections::HashMap;
    let mut old_children: HashMap<&str, &DirNode> = HashMap::new();
    if let Some(o) = old {
        for c in &o.children {
            old_children.insert(c.name.as_str(), c);
        }
    }
    let mut new_children: HashMap<&str, &DirNode> = HashMap::new();
    if let Some(n) = new {
        for c in &n.children {
            new_children.insert(c.name.as_str(), c);
        }
    }
    let mut names: Vec<&str> = old_children
        .keys()
        .chain(new_children.keys())
        .copied()
        .collect();
    names.sort_unstable();
    names.dedup();

    let mut covered: i64 = 0;
    for name in names {
        let oc = old_children.get(name).copied();
        let nc = new_children.get(name).copied();
        let child_delta = alloc_of(nc) - alloc_of(oc);
        if child_delta.abs() >= min_delta {
            covered += child_delta;
            let child_path = format!("{}/{}", path, name);
            attribute(oc, nc, &child_path, min_delta, out);
        } else if alloc_of(oc).max(alloc_of(nc)) >= min_delta {
            // 자식 자체 변화는 작아도 그 아래에서 상쇄된 큰 변화(+X/-X)가 있을 수 있다.
            let child_path = format!("{}/{}", path, name);
            attribute(oc, nc, &child_path, min_delta, out);
        }
    }

    let residual = delta - covered;
    if residual.abs() >= min_delta {
        out.push(DiffEntry {
            path: path.to_string(),
            delta: residual,
            before_total: before as u64,
            after_total: after as u64,
            is_residual: covered != 0,
        });
    }
}

#[cfg(test)]
mod diff_tests {
    use super::*;
    use space_scanner::{scan, ScanOptions};
    use std::fs;

    #[test]
    fn diff_attributes_growth_to_culprit_dir() {
        let tmp = std::env::temp_dir().join(format!("space-diff-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("stable")).unwrap();
        fs::create_dir_all(tmp.join("growing/deep")).unwrap();
        fs::write(tmp.join("stable/base.bin"), vec![0u8; 100_000]).unwrap();
        fs::write(tmp.join("growing/deep/old.bin"), vec![0u8; 50_000]).unwrap();

        let db = std::env::temp_dir().join(format!("space-diff-db-{}.db", std::process::id()));
        let _ = fs::remove_file(&db);
        let mut conn = open(&db).unwrap();

        let scan1 = scan(&tmp, ScanOptions::default()).unwrap();
        let id1 = save_snapshot(&mut conn, &tmp, &scan1).unwrap();

        // growing/deep에 큰 파일 추가, stable은 그대로.
        fs::write(tmp.join("growing/deep/new.bin"), vec![1u8; 500_000]).unwrap();
        let scan2 = scan(&tmp, ScanOptions::default()).unwrap();
        let id2 = save_snapshot(&mut conn, &tmp, &scan2).unwrap();

        let entries = diff_snapshots(&conn, id1, id2, 100_000).unwrap();
        assert_eq!(entries.len(), 1, "{:?}", entries);
        // 범인은 최상위가 아니라 실제 변화가 생긴 깊은 디렉토리로 귀속되어야 한다.
        assert!(
            entries[0].path.ends_with("growing/deep"),
            "{}",
            entries[0].path
        );
        assert!(entries[0].delta >= 500_000);

        // 목록/개별 로드도 확인.
        let snaps = list_snapshots(&conn, &tmp).unwrap();
        assert_eq!(snaps.len(), 2);
        assert!(load_by_id(&conn, id2).unwrap().is_some());

        drop(conn);
        let _ = fs::remove_dir_all(&tmp);
        let _ = fs::remove_file(&db);
    }
}

// ───────────────────────── git 상태 캐시 (TTL + signature) ─────────────────────────

/// git repo 건강 상태 캐시. 키 = (git_sig, tree_sig). 둘 중 하나라도 바뀌면 stale.
/// git_sig = .git 내부 mtime(커밋/ref/staging), tree_sig = 스캔 트리의 file_count·alloc
/// (working-tree 파일 추가/삭제/크기변화 = dirty 원인 대부분을 잡는 보조 키).
pub fn git_cache_open(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS git_cache (
            repo_path TEXT PRIMARY KEY,
            git_sig INTEGER NOT NULL,
            tree_sig INTEGER NOT NULL,
            health_json TEXT NOT NULL,
            computed_at INTEGER NOT NULL
        );",
    )
}

/// 캐시 조회. signature 일치 + TTL 이내면 health_json 반환.
pub fn git_cache_get(
    conn: &Connection,
    repo_path: &str,
    git_sig: u64,
    tree_sig: u64,
    ttl_secs: u64,
    now_secs: u64,
) -> Option<String> {
    conn.query_row(
        "SELECT git_sig, tree_sig, health_json, computed_at FROM git_cache WHERE repo_path = ?1",
        rusqlite::params![repo_path],
        |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? as u64,
            ))
        },
    )
    .ok()
    .and_then(|(g, t, json, at)| {
        let fresh = g == git_sig && t == tree_sig && now_secs.saturating_sub(at) < ttl_secs;
        if fresh {
            Some(json)
        } else {
            None
        }
    })
}

/// 캐시 저장(upsert).
pub fn git_cache_put(
    conn: &Connection,
    repo_path: &str,
    git_sig: u64,
    tree_sig: u64,
    health_json: &str,
    now_secs: u64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO git_cache (repo_path, git_sig, tree_sig, health_json, computed_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(repo_path) DO UPDATE SET
           git_sig=excluded.git_sig, tree_sig=excluded.tree_sig,
           health_json=excluded.health_json, computed_at=excluded.computed_at",
        rusqlite::params![
            repo_path,
            git_sig as i64,
            tree_sig as i64,
            health_json,
            now_secs as i64
        ],
    )?;
    Ok(())
}
