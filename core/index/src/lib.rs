//! 스캔 결과의 SQLite 스냅샷 캐시.
//!
//! 스캔 트리(디렉토리 집계 + 대용량 파일)를 저장해 앱/CLI 재실행 시
//! 재스캔 없이 이전 상태를 즉시 로드한다. FSEvents 증분 반영은 M4에서 이 위에 얹는다.

pub use rusqlite;
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

pub fn open(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
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
            allocated_size INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_big_files_scan ON big_files(scan_id);",
    )?;
    migrate(&conn)?;
    Ok(conn)
}

/// 스키마 버전. PRAGMA user_version으로 추적한다 (0 = Phase 1 초기 스키마).
const SCHEMA_VERSION: i64 = 1;

/// user_version 기반 순차 마이그레이션. 각 단계는 멱등적이지 않아도 되지만
/// (버전 검사로 1회만 실행) 실패 시 버전을 올리지 않아 다음 open에서 재시도된다.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let mut version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        // v1: 회수 실행 이력 (F1). 예상 vs 측정 회수량을 남겨 '변화' 탭 마커로 쓴다.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS reclaim_log (
                id INTEGER PRIMARY KEY,
                executed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
                root_path TEXT NOT NULL,
                item_count INTEGER NOT NULL,
                estimated INTEGER NOT NULL,
                measured INTEGER,
                undone INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        version = 1;
        conn.pragma_update(None, "user_version", version)?;
    }
    debug_assert_eq!(version, SCHEMA_VERSION);
    Ok(())
}

// ───────────────────────── 회수 이력 (F1) ─────────────────────────

pub struct ReclaimRecord {
    pub id: i64,
    pub executed_at: String,
    pub root_path: String,
    pub item_count: u64,
    pub estimated: u64,
    /// 실행 후 증분 재스캔으로 측정한 실제 회수량. None = 측정 실패/미측정.
    pub measured: Option<i64>,
    pub undone: bool,
}

/// 회수 실행 기록을 남기고 id를 반환한다. measured는 검증 후 update로 채운다.
pub fn reclaim_log_add(
    conn: &Connection,
    root_path: &Path,
    item_count: u64,
    estimated: u64,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO reclaim_log (root_path, item_count, estimated) VALUES (?1, ?2, ?3)",
        params![
            root_path.to_string_lossy().into_owned(),
            item_count as i64,
            estimated as i64
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 검증 재스캔이 끝난 뒤 측정 회수량을 기록한다 (양수 = 실제로 줄어든 양).
pub fn reclaim_log_set_measured(conn: &Connection, id: i64, measured: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE reclaim_log SET measured = ?2 WHERE id = ?1",
        params![id, measured],
    )?;
    Ok(())
}

/// undo 실행 표시.
pub fn reclaim_log_set_undone(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE reclaim_log SET undone = 1 WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// 해당 루트의 회수 이력 (최신순, limit개).
pub fn reclaim_log_list(
    conn: &Connection,
    root_path: &Path,
    limit: u32,
) -> rusqlite::Result<Vec<ReclaimRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, executed_at, root_path, item_count, estimated, measured, undone
         FROM reclaim_log WHERE root_path = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(
        params![root_path.to_string_lossy().into_owned(), limit as i64],
        |r| {
            Ok(ReclaimRecord {
                id: r.get(0)?,
                executed_at: r.get(1)?,
                root_path: r.get(2)?,
                item_count: r.get::<_, i64>(3)? as u64,
                estimated: r.get::<_, i64>(4)? as u64,
                measured: r.get(5)?,
                undone: r.get::<_, i64>(6)? != 0,
            })
        },
    )?;
    rows.collect()
}

/// 스캔 결과 전체를 하나의 트랜잭션으로 저장하고 scan_id를 반환한다.
pub fn save_snapshot(
    conn: &mut Connection,
    root_path: &Path,
    result: &ScanResult,
) -> rusqlite::Result<i64> {
    save_tree(
        conn,
        root_path,
        &result.root,
        result.stats.total_files,
        result.stats.total_dirs,
    )
}

/// 트리 참조로 저장하는 변형 — 증분 갱신된(소유권 없는) 트리를 스냅샷으로 남길 때 쓴다.
pub fn save_tree(
    conn: &mut Connection,
    root_path: &Path,
    root: &DirNode,
    total_files: u64,
    total_dirs: u64,
) -> rusqlite::Result<i64> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO scans (root_path, total_files, total_dirs) VALUES (?1, ?2, ?3)",
        params![
            root_path.to_string_lossy(),
            total_files as i64,
            total_dirs as i64
        ],
    )?;
    let scan_id = tx.last_insert_rowid();
    {
        let mut node_stmt = tx.prepare(
            "INSERT INTO nodes (scan_id, node_id, parent_id, name, logical_size, allocated_size, file_count, dir_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        let mut file_stmt = tx.prepare(
            "INSERT INTO big_files (scan_id, node_id, path, logical_size, allocated_size)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;

        // 명시적 스택으로 순회 (트리 깊이에 따른 콜스택 부담 회피).
        let mut next_id: i64 = 0;
        let mut stack: Vec<(&DirNode, Option<i64>)> = vec![(root, None)];
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
                    f.allocated_size as i64
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
        "SELECT node_id, path, logical_size, allocated_size FROM big_files WHERE scan_id = ?1",
    )?;
    let files = fstmt.query_map(params![scan_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            FileEntry {
                path: PathBuf::from(row.get::<_, String>(1)?),
                logical_size: row.get::<_, i64>(2)? as u64,
                allocated_size: row.get::<_, i64>(3)? as u64,
            },
        ))
    })?;
    for f in files {
        let (node_id, entry) = f?;
        big.entry(node_id).or_default().push(entry);
    }

    // node_id -> children 목록을 만들고 루트(parent NULL)부터 조립.
    let mut children_of: HashMap<i64, Vec<Row>> = HashMap::new();
    let mut root_row: Option<Row> = None;
    for r in rows {
        match r.parent_id {
            None => root_row = Some(r),
            Some(p) => children_of.entry(p).or_default().push(r),
        }
    }
    fn attach(
        row: &mut Row,
        children_of: &mut HashMap<i64, Vec<Row>>,
        big: &mut HashMap<i64, Vec<FileEntry>>,
    ) {
        if let Some(files) = big.remove(&row.node_id) {
            row.node.big_files = files;
        }
        if let Some(mut kids) = children_of.remove(&row.node_id) {
            for k in &mut kids {
                attach(k, children_of, big);
            }
            row.node.children = kids.into_iter().map(|k| k.node).collect();
        }
    }

    let Some(mut root_row) = root_row else {
        return Ok(None);
    };
    attach(&mut root_row, &mut children_of, &mut big);
    Ok(Some(root_row.node))
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

        drop(conn);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reclaim_log_roundtrip_and_migration() {
        let tmp = std::env::temp_dir().join(format!("space-index-rl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let db = tmp.join("snap.db");

        let conn = open(&db).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        let root = Path::new("/tmp/example-root");
        let id = reclaim_log_add(&conn, root, 3, 1_000_000).unwrap();
        reclaim_log_set_measured(&conn, id, 900_000).unwrap();

        let records = reclaim_log_list(&conn, root, 10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].item_count, 3);
        assert_eq!(records[0].estimated, 1_000_000);
        assert_eq!(records[0].measured, Some(900_000));
        assert!(!records[0].undone);

        reclaim_log_set_undone(&conn, id).unwrap();
        assert!(reclaim_log_list(&conn, root, 10).unwrap()[0].undone);

        // 재-open해도 마이그레이션이 멱등이어야 한다.
        drop(conn);
        let conn = open(&db).unwrap();
        assert_eq!(reclaim_log_list(&conn, root, 10).unwrap().len(), 1);

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
    out.sort_by(|a, b| b.delta.abs().cmp(&a.delta.abs()));
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
