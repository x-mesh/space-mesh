//! SwiftUI 앱을 위한 UniFFI 바인딩.
//!
//! 트리는 재귀 Record로 넘기지 않고(ScanHandle이 소유), Swift는 index path(Vec<u32>)로
//! 레벨 단위 조회한다 — 수십만 노드를 FFI 경계 너머로 복사하지 않기 위한 설계.

use space_scanner::{scan_with_progress, DirNode, ScanOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, RwLock};

uniffi::setup_scaffolding!();

/// 진행 중인 스캔/해시 작업의 라이브 카운터 — UI가 폴링한다.
static PROGRESS: LazyLock<Arc<AtomicU64>> = LazyLock::new(|| Arc::new(AtomicU64::new(0)));

/// 현재 작업이 처리한 파일 수 (스캔: 발견한 파일, dedup: 해시한 파일).
#[uniffi::export]
pub fn scan_progress() -> u64 {
    PROGRESS.load(Ordering::Relaxed)
}

fn reset_progress() -> Arc<AtomicU64> {
    PROGRESS.store(0, Ordering::Relaxed);
    Arc::clone(&PROGRESS)
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ScanError {
    #[error("io error: {msg}")]
    Io { msg: String },
    #[error("invalid node path")]
    InvalidPath,
    #[error("snapshot error: {msg}")]
    Snapshot { msg: String },
}

/// 한 노드의 표시용 정보. children은 포함하지 않는다 (레벨 단위 조회).
#[derive(uniffi::Record)]
pub struct NodeInfo {
    /// 부모의 children 내 원본 index — drilldown 시 index path 구성에 사용.
    pub index: u32,
    pub name: String,
    pub logical_size: u64,
    pub allocated_size: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub child_count: u32,
    /// 서브트리 최신 파일 mtime (unix 초). 0 = 미상 (나이 오버레이용, F5).
    pub newest_mtime: i64,
}

#[derive(uniffi::Record)]
pub struct BigFile {
    pub path: String,
    pub logical_size: u64,
    pub allocated_size: u64,
    /// 파일 mtime (unix 초). 0 = 미상.
    pub mtime: i64,
}

#[derive(uniffi::Record, Clone)]
pub struct ScanStatsInfo {
    pub total_files: u64,
    pub total_dirs: u64,
    pub errors: u64,
    /// errors 중 권한 거부(EACCES/EPERM) — Full Disk Access 안내 판단용.
    pub permission_errors: u64,
}

/// 스캔 트리 핸들. refresh_paths(F2)로 서브트리 단위 갱신이 가능해
/// 내부 상태를 RwLock으로 보호한다 — 조회는 동시, 갱신은 배타.
#[derive(uniffi::Object)]
pub struct ScanHandle {
    root_path: PathBuf,
    root: RwLock<DirNode>,
    stats: RwLock<ScanStatsInfo>,
}

fn resolve<'a>(root: &'a DirNode, index_path: &[u32]) -> Result<&'a DirNode, ScanError> {
    let mut node = root;
    for &i in index_path {
        node = node
            .children
            .get(i as usize)
            .ok_or(ScanError::InvalidPath)?;
    }
    Ok(node)
}

fn to_info(index: u32, node: &DirNode) -> NodeInfo {
    NodeInfo {
        index,
        name: node.name.clone(),
        logical_size: node.logical_size,
        allocated_size: node.allocated_size,
        file_count: node.file_count,
        dir_count: node.dir_count,
        child_count: node.children.len() as u32,
        newest_mtime: node.newest_mtime,
    }
}

#[uniffi::export]
impl ScanHandle {
    /// index path가 가리키는 노드의 정보.
    pub fn node_at(&self, index_path: Vec<u32>) -> Result<NodeInfo, ScanError> {
        let root = self.root.read().unwrap();
        let idx = index_path.last().copied().unwrap_or(0);
        Ok(to_info(idx, resolve(&root, &index_path)?))
    }

    /// 해당 노드의 자식 디렉토리 목록 (allocated 내림차순, 원본 index 포함).
    pub fn children(&self, index_path: Vec<u32>) -> Result<Vec<NodeInfo>, ScanError> {
        let root = self.root.read().unwrap();
        let node = resolve(&root, &index_path)?;
        let mut infos: Vec<NodeInfo> = node
            .children
            .iter()
            .enumerate()
            .map(|(i, c)| to_info(i as u32, c))
            .collect();
        infos.sort_by(|a, b| b.allocated_size.cmp(&a.allocated_size));
        Ok(infos)
    }

    /// 해당 노드 직속의 대용량 파일 (allocated 내림차순).
    pub fn big_files_at(&self, index_path: Vec<u32>) -> Result<Vec<BigFile>, ScanError> {
        let root = self.root.read().unwrap();
        let node = resolve(&root, &index_path)?;
        let mut files: Vec<BigFile> = node
            .big_files
            .iter()
            .map(|f| BigFile {
                path: f.path.to_string_lossy().into_owned(),
                logical_size: f.logical_size,
                allocated_size: f.allocated_size,
                mtime: f.mtime,
            })
            .collect();
        files.sort_by(|a, b| b.allocated_size.cmp(&a.allocated_size));
        Ok(files)
    }

    /// 트리 전체에서 가장 큰 파일 top-N.
    pub fn top_files(&self, limit: u32) -> Vec<BigFile> {
        let root = self.root.read().unwrap();
        space_scanner::top_files(&root, limit as usize)
            .into_iter()
            .map(|f| BigFile {
                path: f.path.to_string_lossy().into_owned(),
                logical_size: f.logical_size,
                allocated_size: f.allocated_size,
                mtime: f.mtime,
            })
            .collect()
    }

    /// index path를 실제 파일시스템 경로 문자열로 변환 (Finder 표시/Quick Look용).
    pub fn full_path(&self, index_path: Vec<u32>) -> Result<String, ScanError> {
        let root = self.root.read().unwrap();
        let mut path = self.root_path.clone();
        let mut node = &*root;
        for &i in &index_path {
            node = node
                .children
                .get(i as usize)
                .ok_or(ScanError::InvalidPath)?;
            path.push(&node.name);
        }
        Ok(path.to_string_lossy().into_owned())
    }

    pub fn stats(&self) -> ScanStatsInfo {
        self.stats.read().unwrap().clone()
    }
}

// ───────────────────────── F2: 증분 재스캔 ─────────────────────────

/// refresh_paths 한 번의 요약 — UI 상태 표시와 검증 리포트(F1)에 쓴다.
#[derive(uniffi::Record)]
pub struct RefreshSummary {
    /// 실제로 재스캔한 서브트리 수 (정규화 후).
    pub refreshed_subtrees: u32,
    /// 트리 전체 allocated 변화량 (음수 = 감소).
    pub delta_allocated: i64,
    /// 재스캔 중 건너뛴 항목 수 (권한 등).
    pub errors: u64,
    pub elapsed_ms: u64,
}

#[uniffi::export]
impl ScanHandle {
    /// 절대 경로 목록이 가리키는 서브트리만 다시 스캔해 트리를 갱신한다.
    /// 루트 밖 경로는 무시하고, 포함 관계인 경로는 최상위만 재스캔한다.
    ///
    /// - 경로 매칭은 루트의 원 표기와 canonicalize된 실경로 양쪽을 시도한다 —
    ///   FSEvents는 심볼릭 링크가 해석된 경로(/private/tmp/…)를 전달하기 때문.
    /// - 디스크 스캔은 락 없이 수행하고, 트리 접합만 짧은 쓰기 락으로 처리해
    ///   UI 조회가 스캔 시간만큼 블로킹되지 않는다.
    ///
    /// 블로킹(서브트리 크기에 비례) — Swift는 백그라운드에서 직렬로 호출할 것.
    pub fn refresh_paths(
        &self,
        abs_paths: Vec<String>,
        min_file_mib: u64,
    ) -> Result<RefreshSummary, ScanError> {
        use space_scanner::{
            deepest_existing_path, rel_segments, remove_missing, scan_subtree, splice_subtree,
        };

        let started = std::time::Instant::now();
        let opts = ScanOptions {
            record_file_threshold: min_file_mib * 1024 * 1024,
            ..Default::default()
        };
        let canonical_root = std::fs::canonicalize(&self.root_path).ok();

        // 루트 기준 상대 경로로 정규화 (원 표기 → 실경로 순으로 매칭).
        let mut rels: Vec<PathBuf> = Vec::new();
        for p in &abs_paths {
            let p = Path::new(p);
            let rel = p.strip_prefix(&self.root_path).ok().or_else(|| {
                canonical_root
                    .as_deref()
                    .and_then(|c| p.strip_prefix(c).ok())
            });
            match rel {
                Some(rel) => rels.push(rel.to_path_buf()),
                None => continue, // 루트 밖 — 무시
            }
        }
        // 포함 관계 정규화: 정렬 후, 직전에 남긴 경로의 하위면 건너뛴다.
        rels.sort();
        let mut targets: Vec<PathBuf> = Vec::new();
        for rel in rels {
            if let Some(last) = targets.last() {
                if last.as_os_str().is_empty() || rel.starts_with(last) {
                    continue;
                }
            }
            targets.push(rel);
        }

        let mut delta_allocated = 0i64;
        let mut delta_files = 0i64;
        let mut delta_dirs = 0i64;
        let mut errors = 0u64;
        let mut refreshed = 0u32;
        for rel in &targets {
            let segments = match rel_segments(rel) {
                Ok(s) => s,
                Err(_) => {
                    errors += 1;
                    continue;
                }
            };
            // ① 짧은 읽기 락: 트리에 존재하는 가장 깊은 조상을 찾는다.
            let names = {
                let root = self.root.read().unwrap();
                deepest_existing_path(&root, &segments)
            };
            let disk_path: PathBuf = self.root_path.join(names.iter().collect::<PathBuf>());

            // ② 락 없이 디스크 스캔 — UI 조회는 이 동안 자유롭게 진행된다.
            let scanned = match std::fs::symlink_metadata(&disk_path) {
                Ok(md) if md.is_dir() => {
                    let name = names
                        .last()
                        .cloned()
                        .unwrap_or_else(|| self.root.read().unwrap().name.clone());
                    match scan_subtree(&disk_path, name, &opts) {
                        Ok((fresh, errs)) => {
                            errors += errs;
                            Some(Some(fresh))
                        }
                        Err(_) => {
                            errors += 1;
                            None
                        }
                    }
                }
                Ok(_) | Err(_) if names.is_empty() => {
                    return Err(ScanError::Io {
                        msg: "scan root disappeared or unreadable".into(),
                    });
                }
                Ok(_) => Some(None), // 더 이상 디렉토리가 아님 → 제거
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None), // 사라짐 → 제거
                // 권한/IO 일시 오류 — 삭제로 오인하지 않고 트리 유지.
                Err(_) => {
                    errors += 1;
                    None
                }
            };
            let Some(fresh) = scanned else { continue };

            // ③ 짧은 쓰기 락: 접합 + 조상 델타 갱신.
            let mut root = self.root.write().unwrap();
            let delta = match fresh {
                Some(fresh) => splice_subtree(&mut root, &names, Some(fresh)),
                // 제거는 사라짐이 시작된 최상위 조상부터 (조상별 stat 몇 번 — 짧음).
                None => Some(remove_missing(&mut root, &self.root_path, &names)),
            };
            if let Some(d) = delta {
                delta_allocated += d.allocated;
                delta_files += d.files;
                delta_dirs += d.dirs;
                errors += d.errors;
                refreshed += 1;
            }
        }

        let mut stats = self.stats.write().unwrap();
        stats.total_files = stats.total_files.saturating_add_signed(delta_files);
        stats.total_dirs = stats.total_dirs.saturating_add_signed(delta_dirs);
        stats.errors += errors;

        Ok(RefreshSummary {
            refreshed_subtrees: refreshed,
            delta_allocated,
            errors,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    /// 현재 트리(증분 갱신 포함)를 스냅샷으로 저장하고 scan_id를 반환한다.
    /// live 모드가 refresh_paths 후 시계열을 계속 쌓는 데 쓴다.
    pub fn save_to_db(&self, db_path: String) -> Result<i64, ScanError> {
        let mut conn = open_db(&db_path)?;
        let root = self.root.read().unwrap();
        let stats = self.stats.read().unwrap().clone();
        let id = space_index::save_tree(
            &mut conn,
            &self.root_path,
            &root,
            stats.total_files,
            stats.total_dirs,
        )
        .map_err(|e| ScanError::Snapshot { msg: e.to_string() })?;
        // 보존 정책 적용 (F7) — 방금 저장분은 항상 "최근 7일"에 들어 안전.
        let _ = space_index::prune_snapshots(&mut conn);
        Ok(id)
    }
}

/// 경로를 스캔해 핸들을 반환한다. 블로킹 — Swift에서 백그라운드 Task로 호출할 것.
#[uniffi::export]
pub fn scan_path(path: String, min_file_mib: u64) -> Result<Arc<ScanHandle>, ScanError> {
    let root_path = PathBuf::from(&path);
    let opts = ScanOptions {
        record_file_threshold: min_file_mib * 1024 * 1024,
        ..Default::default()
    };
    let result = scan_with_progress(&root_path, opts, Some(reset_progress()))
        .map_err(|e| ScanError::Io { msg: e.to_string() })?;
    Ok(Arc::new(ScanHandle {
        root_path,
        stats: RwLock::new(ScanStatsInfo {
            total_files: result.stats.total_files,
            total_dirs: result.stats.total_dirs,
            errors: result.stats.errors,
            permission_errors: result.stats.permission_errors,
        }),
        root: RwLock::new(result.root),
    }))
}

/// SQLite 스냅샷에서 로드. 없으면 Snapshot 에러.
#[uniffi::export]
pub fn load_snapshot(db_path: String, root_path: String) -> Result<Arc<ScanHandle>, ScanError> {
    let conn = open_db(&db_path)?;
    let loaded = space_index::load_latest(&conn, Path::new(&root_path))
        .map_err(|e| ScanError::Snapshot { msg: e.to_string() })?;
    let Some((meta, root)) = loaded else {
        return Err(ScanError::Snapshot {
            msg: "no snapshot for this root".into(),
        });
    };
    Ok(Arc::new(ScanHandle {
        root_path: PathBuf::from(root_path),
        stats: RwLock::new(ScanStatsInfo {
            total_files: meta.total_files,
            total_dirs: meta.total_dirs,
            errors: 0,
            permission_errors: 0,
        }),
        root: RwLock::new(root),
    }))
}

/// 스캔 후 스냅샷 저장까지 한 번에.
#[uniffi::export]
pub fn scan_and_save(
    path: String,
    min_file_mib: u64,
    db_path: String,
) -> Result<Arc<ScanHandle>, ScanError> {
    let root_path = PathBuf::from(&path);
    let opts = ScanOptions {
        record_file_threshold: min_file_mib * 1024 * 1024,
        ..Default::default()
    };
    let result = scan_with_progress(&root_path, opts, Some(reset_progress()))
        .map_err(|e| ScanError::Io { msg: e.to_string() })?;
    let mut conn = open_db(&db_path)?;
    space_index::save_snapshot(&mut conn, &root_path, &result)
        .map_err(|e| ScanError::Snapshot { msg: e.to_string() })?;
    // 보존 정책 적용 (F7) — 실패해도 스캔 결과에는 영향 없음.
    let _ = space_index::prune_snapshots(&mut conn);
    Ok(Arc::new(ScanHandle {
        root_path,
        stats: RwLock::new(ScanStatsInfo {
            total_files: result.stats.total_files,
            total_dirs: result.stats.total_dirs,
            errors: result.stats.errors,
            permission_errors: result.stats.permission_errors,
        }),
        root: RwLock::new(result.root),
    }))
}

// ───────────────────────── M3: 불필요 파일 탐지 + 중복 탐지 ─────────────────────────

#[derive(uniffi::Record)]
pub struct CleanupCandidate {
    pub rule_id: String,
    pub title: String,
    pub category: String,
    /// "safe" = 원클릭 정리 가능, "warn" = 검토 필요.
    pub safety: String,
    pub description: String,
    pub path: String,
    pub allocated_size: u64,
    pub file_count: u64,
    pub recreate_command: String,
    pub recreate_cost: String,
}

/// 내장 룰셋으로 홈 디렉토리의 정리 후보를 탐지한다. 블로킹 — 백그라운드에서 호출.
#[uniffi::export]
pub fn detect_cleanup(home: String) -> Vec<CleanupCandidate> {
    space_rules::detect(Path::new(&home))
        .into_iter()
        .map(|c| CleanupCandidate {
            rule_id: c.rule.id,
            title: c.rule.title,
            category: c.rule.category,
            safety: c.rule.safety,
            description: c.rule.description,
            path: c.resolved_path.to_string_lossy().into_owned(),
            allocated_size: c.allocated_size,
            file_count: c.file_count,
            recreate_command: c.rule.recreate_command,
            recreate_cost: c.rule.recreate_cost,
        })
        .collect()
}

#[derive(uniffi::Record)]
pub struct DupGroupInfo {
    pub file_size: u64,
    /// 클론 공유 블록 보정 후 회수 가능량.
    pub reclaimable: u64,
    /// true = 이미 APFS 클론으로 블록을 공유하는 파일이 있다 (지워도 회수 0).
    pub clone_shared: bool,
    pub hash_hex: String,
    pub files: Vec<String>,
}

/// root 아래 min_size_mib 이상 파일의 중복 그룹. 블로킹 — 백그라운드에서 호출.
/// 진행 상황은 scan_progress()로 폴링 (해시 처리 파일 수).
#[uniffi::export]
pub fn find_duplicates(root: String, min_size_mib: u64) -> Result<Vec<DupGroupInfo>, ScanError> {
    let result = space_dedup::find_duplicates(
        Path::new(&root),
        min_size_mib.max(1) * 1024 * 1024,
        Some(reset_progress()),
    )
    .map_err(|e| ScanError::Io { msg: e.to_string() })?;
    Ok(result
        .groups
        .into_iter()
        .map(|g| DupGroupInfo {
            file_size: g.file_size,
            reclaimable: g.reclaimable,
            clone_shared: g.clone_shared,
            hash_hex: g.hash_hex,
            files: g
                .files
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        })
        .collect())
}

/// 중복 그룹 병합 결과 (F3).
#[derive(uniffi::Record)]
pub struct MergeResult {
    pub merged: u32,
    pub failed: u32,
    /// 회수 추정치 상한 (victim들이 점유하던 블록 합).
    pub reclaimed: u64,
}

/// victim들을 keep의 APFS 클론 사본으로 교체한다 — 데이터 손실 없는 회수.
/// keep은 배치당 1회만 해시하고, 각 victim은 교체 직전 재해시로 동일성을
/// 재확인한다. 실패한 항목은 무손상으로 남는다. 블로킹(해시 IO) — 백그라운드에서 호출.
#[uniffi::export]
pub fn merge_duplicates(keep: String, victims: Vec<String>) -> MergeResult {
    let victims: Vec<PathBuf> = victims.into_iter().map(PathBuf::from).collect();
    let stats = space_dedup::merge_group(Path::new(&keep), &victims);
    MergeResult {
        merged: stats.merged,
        failed: stats.failed,
        reclaimed: stats.reclaimed,
    }
}

// ───────────────────────── 카테고리 뷰 (스캔 트리 재사용) ─────────────────────────

#[derive(uniffi::Record)]
pub struct CategoryHitInfo {
    pub category_id: String,
    pub title: String,
    pub safety: String,
    pub description: String,
    pub path: String,
    pub project_path: String,
    pub allocated_size: u64,
    pub file_count: u64,
    pub verified: bool,
    pub recreate_command: String,
    pub recreate_cost: String,
    /// 프로젝트의 git 마지막 커밋으로부터 지난 일수 (git 없으면 None).
    pub idle_days: Option<u64>,
}

#[uniffi::export]
impl ScanHandle {
    /// 스캔된 트리에서 잘 알려진 산출물 카테고리(node_modules, cargo target 등)를 찾는다.
    /// 트리는 메모리에 있어 즉시 반환된다 (마커 확인만 파일시스템 조회).
    pub fn categories(&self) -> Vec<CategoryHitInfo> {
        let root = self.root.read().unwrap();
        let hits = space_rules::categories::find_categories(&root, &self.root_path);
        let idle = space_rules::categories::annotate_idle(&hits);
        hits.into_iter()
            .map(|h| CategoryHitInfo {
                category_id: h.def.id.to_string(),
                title: h.def.title.to_string(),
                safety: h.def.safety.to_string(),
                description: h.def.description.to_string(),
                path: h.path.to_string_lossy().into_owned(),
                project_path: h.project_path.to_string_lossy().into_owned(),
                allocated_size: h.allocated_size,
                file_count: h.file_count,
                verified: h.verified,
                recreate_command: h.def.recreate_command.to_string(),
                recreate_cost: h.def.recreate_cost.to_string(),
                idle_days: idle.get(&h.project_path).copied(),
            })
            .collect()
    }
}

// ───────────────────────── 스냅샷 diff + 툴 어드바이저 ─────────────────────────

#[derive(uniffi::Record)]
pub struct SnapshotInfo {
    pub scan_id: i64,
    pub created_at: String,
    pub total_files: u64,
    pub total_dirs: u64,
}

#[derive(uniffi::Record)]
pub struct DiffEntryInfo {
    /// 루트 이름부터의 상대 경로.
    pub path: String,
    /// 이 항목에 귀속된 변화량 (음수 = 감소).
    pub delta: i64,
    pub before_total: u64,
    pub after_total: u64,
    /// true면 하위로 설명되지 않는 잔차(직속 파일 변화).
    pub is_residual: bool,
}

/// 해당 루트의 스냅샷 목록 (최신순).
#[uniffi::export]
pub fn list_snapshots(db_path: String, root_path: String) -> Result<Vec<SnapshotInfo>, ScanError> {
    let conn = open_db(&db_path)?;
    let snaps = space_index::list_snapshots(&conn, Path::new(&root_path))
        .map_err(|e| ScanError::Snapshot { msg: e.to_string() })?;
    Ok(snaps
        .into_iter()
        .map(|s| SnapshotInfo {
            scan_id: s.scan_id,
            created_at: s.created_at,
            total_files: s.total_files,
            total_dirs: s.total_dirs,
        })
        .collect())
}

/// 두 스냅샷 비교 — 변화의 범인 목록 (|delta| 내림차순).
#[uniffi::export]
pub fn diff_snapshots(
    db_path: String,
    old_id: i64,
    new_id: i64,
    min_delta_mib: u64,
) -> Result<Vec<DiffEntryInfo>, ScanError> {
    let conn = open_db(&db_path)?;
    let entries = space_index::diff_snapshots(&conn, old_id, new_id, min_delta_mib * 1024 * 1024)
        .map_err(|e| ScanError::Snapshot { msg: e.to_string() })?;
    Ok(entries
        .into_iter()
        .map(|e| DiffEntryInfo {
            path: e.path,
            delta: e.delta,
            before_total: e.before_total,
            after_total: e.after_total,
            is_residual: e.is_residual,
        })
        .collect())
}

#[derive(uniffi::Record)]
pub struct ToolAdviceInfo {
    pub tool: String,
    pub command: String,
    pub description: String,
    pub estimated_reclaim: Option<u64>,
    pub available: bool,
    pub detail: String,
}

/// 설치된 도구들의 공식 정리 커맨드 제안. 블로킹(dry-run 실행) — 백그라운드에서 호출.
#[uniffi::export]
pub fn tool_advice() -> Vec<ToolAdviceInfo> {
    space_rules::advisor::advise()
        .into_iter()
        .map(|a| ToolAdviceInfo {
            tool: a.tool,
            command: a.command,
            description: a.description,
            estimated_reclaim: a.estimated_reclaim,
            available: a.available,
            detail: a.detail,
        })
        .collect()
}

// ───────────────────────── DiffHandle: drilldown 탐색 ─────────────────────────

/// 두 스냅샷 트리를 메모리에 상주시켜 레벨 단위 diff 탐색을 지원한다.
#[derive(uniffi::Object)]
pub struct DiffHandle {
    old: Option<DirNode>,
    new: Option<DirNode>,
}

/// 한 레벨의 행 하나의 변화.
/// kind: "dir" = 하위 디렉토리, "file" = 이 디렉토리 직속의 개별 파일(스캔 시
/// 기록 임계값 이상), "rest" = 임계값 미만 파일들의 변화 합(요약 행).
#[derive(uniffi::Record)]
pub struct DiffChildInfo {
    pub name: String,
    pub before: u64,
    pub after: u64,
    pub delta: i64,
    pub has_children: bool,
    pub kind: String,
}

#[uniffi::export]
pub fn open_diff(db_path: String, old_id: i64, new_id: i64) -> Result<Arc<DiffHandle>, ScanError> {
    let conn = open_db(&db_path)?;
    let old = space_index::load_by_id(&conn, old_id)
        .map_err(|e| ScanError::Snapshot { msg: e.to_string() })?;
    let new = space_index::load_by_id(&conn, new_id)
        .map_err(|e| ScanError::Snapshot { msg: e.to_string() })?;
    Ok(Arc::new(DiffHandle { old, new }))
}

fn resolve_by_names<'a>(root: Option<&'a DirNode>, path: &[String]) -> Option<&'a DirNode> {
    let mut node = root?;
    for segment in path {
        node = node.children.iter().find(|c| &c.name == segment)?;
    }
    Some(node)
}

#[uniffi::export]
impl DiffHandle {
    /// 잔차 귀속 범인 목록 (기존 flat 뷰와 동일).
    pub fn culprits(&self, min_delta_mib: u64) -> Vec<DiffEntryInfo> {
        space_index::diff_trees(
            self.old.as_ref(),
            self.new.as_ref(),
            min_delta_mib * 1024 * 1024,
        )
        .into_iter()
        .map(|e| DiffEntryInfo {
            path: e.path,
            delta: e.delta,
            before_total: e.before_total,
            after_total: e.after_total,
            is_residual: e.is_residual,
        })
        .collect()
    }

    /// path(루트 아래 이름 세그먼트) 디렉토리의 자식별 변화 + 직속 파일 잔차 행.
    /// |delta| 내림차순, 변화 없는 자식도 포함(탐색 컨텍스트 유지).
    pub fn children(&self, path: Vec<String>) -> Vec<DiffChildInfo> {
        let old_node = resolve_by_names(self.old.as_ref(), &path);
        let new_node = resolve_by_names(self.new.as_ref(), &path);

        use std::collections::HashMap;
        let mut old_children: HashMap<&str, &DirNode> = HashMap::new();
        if let Some(o) = old_node {
            for c in &o.children {
                old_children.insert(c.name.as_str(), c);
            }
        }
        let mut new_children: HashMap<&str, &DirNode> = HashMap::new();
        if let Some(n) = new_node {
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

        let mut rows: Vec<DiffChildInfo> = Vec::with_capacity(names.len() + 1);
        let mut children_before: u64 = 0;
        let mut children_after: u64 = 0;
        for name in names {
            let oc = old_children.get(name).copied();
            let nc = new_children.get(name).copied();
            let before = oc.map(|n| n.allocated_size).unwrap_or(0);
            let after = nc.map(|n| n.allocated_size).unwrap_or(0);
            children_before += before;
            children_after += after;
            rows.push(DiffChildInfo {
                name: name.to_string(),
                before,
                after,
                delta: after as i64 - before as i64,
                has_children: oc.map(|n| !n.children.is_empty()).unwrap_or(false)
                    || nc.map(|n| !n.children.is_empty()).unwrap_or(false),
                kind: "dir".to_string(),
            });
        }

        // 직속 파일: 스캔 때 기록된 개별 파일(big_files, 임계값 이상)을 이름 단위로 diff.
        let mut old_files: HashMap<String, u64> = HashMap::new();
        if let Some(o) = old_node {
            for f in &o.big_files {
                if let Some(name) = f.path.file_name() {
                    old_files.insert(name.to_string_lossy().into_owned(), f.allocated_size);
                }
            }
        }
        let mut new_files: HashMap<String, u64> = HashMap::new();
        if let Some(n) = new_node {
            for f in &n.big_files {
                if let Some(name) = f.path.file_name() {
                    new_files.insert(name.to_string_lossy().into_owned(), f.allocated_size);
                }
            }
        }
        let bf_before_sum: u64 = old_files.values().sum();
        let bf_after_sum: u64 = new_files.values().sum();
        let mut file_names: Vec<&String> = old_files.keys().chain(new_files.keys()).collect();
        file_names.sort_unstable();
        file_names.dedup();
        for name in file_names {
            let before = old_files.get(name).copied().unwrap_or(0);
            let after = new_files.get(name).copied().unwrap_or(0);
            if before == after {
                continue; // 변화 없는 파일은 생략 (디렉토리와 달리 수가 많음)
            }
            rows.push(DiffChildInfo {
                name: name.clone(),
                before,
                after,
                delta: after as i64 - before as i64,
                has_children: false,
                kind: "file".to_string(),
            });
        }

        // 임계값 미만 파일들의 변화 합 — 개별 이름을 알 수 없는 잔여분 (요약 행).
        let node_before = old_node.map(|n| n.allocated_size).unwrap_or(0);
        let node_after = new_node.map(|n| n.allocated_size).unwrap_or(0);
        let rest_before = node_before
            .saturating_sub(children_before)
            .saturating_sub(bf_before_sum);
        let rest_after = node_after
            .saturating_sub(children_after)
            .saturating_sub(bf_after_sum);
        if rest_before != rest_after {
            rows.push(DiffChildInfo {
                name: String::new(), // 표시명은 UI가 결정 (파일명과 혼동 방지)
                before: rest_before,
                after: rest_after,
                delta: rest_after as i64 - rest_before as i64,
                has_children: false,
                kind: "rest".to_string(),
            });
        }
        rows.sort_by(|a, b| b.delta.abs().cmp(&a.delta.abs()));
        rows
    }

    /// path 노드의 전/후 총량 (헤더 표시용).
    pub fn totals(&self, path: Vec<String>) -> DiffChildInfo {
        let old_node = resolve_by_names(self.old.as_ref(), &path);
        let new_node = resolve_by_names(self.new.as_ref(), &path);
        let before = old_node.map(|n| n.allocated_size).unwrap_or(0);
        let after = new_node.map(|n| n.allocated_size).unwrap_or(0);
        DiffChildInfo {
            name: path.last().cloned().unwrap_or_default(),
            before,
            after,
            delta: after as i64 - before as i64,
            has_children: true,
            kind: "dir".to_string(),
        }
    }
}

// ───────────────────────── F6: 정책 기반 회수 제안 ─────────────────────────

#[derive(uniffi::Record)]
pub struct SuggestionItemInfo {
    pub path: String,
    pub title: String,
    /// "rule" | "category"
    pub source: String,
    pub safety: String,
    pub estimated: u64,
    pub recreate_command: String,
    pub idle_days: Option<u64>,
}

#[derive(uniffi::Record)]
pub struct SuggestionInfo {
    pub generated_at: u64,
    pub total_estimated: u64,
    pub below_threshold: bool,
    pub items: Vec<SuggestionItemInfo>,
}

#[uniffi::export]
impl ScanHandle {
    /// 정책 기반 회수 제안 — CLI --suggest와 동일한 단일 정책
    /// (space_rules::suggest). 블로킹(룰 경로 측정 + git 조회) —
    /// 백그라운드에서 호출. 홈은 $HOME 기준.
    pub fn suggestions(&self, idle_days: u64, min_bytes: u64) -> SuggestionInfo {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let root = self.root.read().unwrap();
        let s = space_rules::suggest::build(&root, &self.root_path, &home, idle_days, min_bytes);
        SuggestionInfo {
            generated_at: s.generated_at,
            total_estimated: s.total_estimated,
            below_threshold: s.below_threshold,
            items: s
                .items
                .into_iter()
                .map(|i| SuggestionItemInfo {
                    path: i.path.to_string_lossy().into_owned(),
                    title: i.title,
                    source: i.source,
                    safety: i.safety,
                    estimated: i.estimated,
                    recreate_command: i.recreate_command,
                    idle_days: i.idle_days,
                })
                .collect(),
        }
    }
}

// ───────────────────────── F1: 회수 실행 이력 ─────────────────────────

#[derive(uniffi::Record)]
pub struct ReclaimRecordInfo {
    pub id: i64,
    pub executed_at: String,
    pub item_count: u64,
    pub estimated: u64,
    /// 실행 후 증분 재스캔으로 측정한 실제 회수량 (양수 = 줄어든 양). None = 미측정.
    pub measured: Option<i64>,
    pub undone: bool,
}

fn open_db(db_path: &str) -> Result<space_index::rusqlite::Connection, ScanError> {
    space_index::open(Path::new(db_path)).map_err(|e| ScanError::Snapshot { msg: e.to_string() })
}

/// 회수 실행 기록 생성 — 실행 직전에 호출하고, 검증 후 set_measured로 채운다.
#[uniffi::export]
pub fn reclaim_log_add(
    db_path: String,
    root_path: String,
    item_count: u64,
    estimated: u64,
) -> Result<i64, ScanError> {
    let conn = open_db(&db_path)?;
    space_index::reclaim_log_add(&conn, Path::new(&root_path), item_count, estimated)
        .map_err(|e| ScanError::Snapshot { msg: e.to_string() })
}

#[uniffi::export]
pub fn reclaim_log_set_measured(db_path: String, id: i64, measured: i64) -> Result<(), ScanError> {
    let conn = open_db(&db_path)?;
    space_index::reclaim_log_set_measured(&conn, id, measured)
        .map_err(|e| ScanError::Snapshot { msg: e.to_string() })
}

#[uniffi::export]
pub fn reclaim_log_set_undone(db_path: String, id: i64) -> Result<(), ScanError> {
    let conn = open_db(&db_path)?;
    space_index::reclaim_log_set_undone(&conn, id)
        .map_err(|e| ScanError::Snapshot { msg: e.to_string() })
}

/// 해당 루트의 회수 이력 (최신순).
#[uniffi::export]
pub fn reclaim_log_list(
    db_path: String,
    root_path: String,
    limit: u32,
) -> Result<Vec<ReclaimRecordInfo>, ScanError> {
    let conn = open_db(&db_path)?;
    let records = space_index::reclaim_log_list(&conn, Path::new(&root_path), limit)
        .map_err(|e| ScanError::Snapshot { msg: e.to_string() })?;
    Ok(records
        .into_iter()
        .map(|r| ReclaimRecordInfo {
            id: r.id,
            executed_at: r.executed_at,
            item_count: r.item_count,
            estimated: r.estimated,
            measured: r.measured,
            undone: r.undone,
        })
        .collect())
}

// ───────────────────────── git repo 건강도 (t2) ─────────────────────────

#[derive(uniffi::Record)]
pub struct GitRepoInfo {
    pub path: String,
    /// "active" | "caution" | "abandoned" | "danger" | "info"
    pub risk: String,
    /// "branch:<name>" | "detached" | "unborn"
    pub head: String,
    /// ahead 커밋 수 (upstream 있고 ahead>0일 때만 >0). upstream 없으면 no_upstream=true.
    pub ahead: u64,
    pub no_upstream: bool,
    pub has_remote: bool,
    pub tracked_dirty: u64,
    pub untracked_present: bool,
    pub stash_count: u64,
    pub last_commit_days: Option<u64>,
    /// remote-tracking ref가 며칠 전인지 (stale 경고용). None = 모름/remote 없음.
    pub remote_stale_days: Option<u64>,
    pub partial: bool,
}

/// 조회 실패한 repo — 원인별 해결 힌트를 위해 분리.
#[derive(uniffi::Record)]
pub struct GitProbeFailure {
    pub path: String,
    /// "git_missing" | "timeout" | "permission_denied" | "not_a_repo" | "corrupted"
    pub reason: String,
}

#[derive(uniffi::Record)]
pub struct GitReport {
    pub repos: Vec<GitRepoInfo>,
    pub failures: Vec<GitProbeFailure>,
}

/// 스캔 트리에서 .git을 가진 노드의 (경로, tree_sig)를 수집한다.
/// tree_sig = repo 서브트리의 file_count·allocated_size 조합 — working-tree 변경
/// (파일 추가/삭제/크기변화)을 캐시 무효화 보조 키로 쓴다.
fn collect_git_candidates(node: &DirNode, path: &std::path::Path, out: &mut Vec<(PathBuf, u64)>) {
    let has_dot_git = node.children.iter().any(|c| c.name == ".git");
    if has_dot_git {
        let tree_sig = node
            .file_count
            .wrapping_mul(1_000_003)
            .wrapping_add(node.allocated_size);
        out.push((path.to_path_buf(), tree_sig));
    }
    for child in &node.children {
        if child.name == ".git" {
            continue; // .git 내부는 순회 안 함
        }
        collect_git_candidates(child, &path.join(&child.name), out);
    }
}

/// GitRepoInfo ↔ 캐시 JSON 직렬화 (수동 — uniffi Record는 serde 미지원).
fn repo_info_to_json(r: &GitRepoInfo) -> String {
    // 파이프 구분 flat 인코딩 (파싱 단순·경로 미포함).
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        r.risk,
        r.head.replace('|', "/"),
        r.ahead,
        r.no_upstream as u8,
        r.has_remote as u8,
        r.tracked_dirty,
        r.untracked_present as u8,
        r.stash_count,
        r.last_commit_days.map(|d| d as i64).unwrap_or(-1),
        r.remote_stale_days.map(|d| d as i64).unwrap_or(-1),
    )
}

fn repo_info_from_json(path: &str, s: &str) -> Option<GitRepoInfo> {
    let p: Vec<&str> = s.split('|').collect();
    if p.len() != 10 {
        return None;
    }
    let days = |v: &str| -> Option<u64> {
        match v.parse::<i64>().ok()? {
            n if n < 0 => None,
            n => Some(n as u64),
        }
    };
    Some(GitRepoInfo {
        path: path.to_string(),
        risk: p[0].to_string(),
        head: p[1].to_string(),
        ahead: p[2].parse().ok()?,
        no_upstream: p[3] == "1",
        has_remote: p[4] == "1",
        tracked_dirty: p[5].parse().ok()?,
        untracked_present: p[6] == "1",
        stash_count: p[7].parse().ok()?,
        last_commit_days: days(p[8]),
        remote_stale_days: days(p[9]),
        partial: false,
    })
}

fn health_to_info(h: &space_git::RepoHealth) -> GitRepoInfo {
    let (head, ahead, no_upstream) = describe(h);
    GitRepoInfo {
        path: h.path.to_string_lossy().into_owned(),
        risk: risk_str(&h.risk).to_string(),
        head,
        ahead,
        no_upstream,
        has_remote: h.has_remote,
        tracked_dirty: h.tracked_dirty,
        untracked_present: h.untracked_present,
        stash_count: h.stash_count,
        last_commit_days: h.last_commit_days,
        remote_stale_days: h.remote_stale_days,
        partial: h.partial,
    }
}

fn risk_str(r: &space_git::Risk) -> &'static str {
    use space_git::Risk::*;
    match r {
        Danger => "danger",
        Caution => "caution",
        Abandoned => "abandoned",
        Active => "active",
        Info => "info",
    }
}

fn reason_str(e: &space_git::ProbeError) -> &'static str {
    use space_git::ProbeError::*;
    match e {
        GitMissing => "git_missing",
        Timeout => "timeout",
        PermissionDenied => "permission_denied",
        NotARepo => "not_a_repo",
        Corrupted => "corrupted",
    }
}

#[uniffi::export]
impl ScanHandle {
    /// 스캔 트리 안의 git repo를 발견하고 상태를 병렬 조회한다.
    /// include_submodules=false면 submodule/worktree/known-ignore 하위 .git 제외.
    /// 블로킹(git 프로세스) — Swift는 백그라운드 Task로 호출.
    pub fn git_repos(&self, include_submodules: bool) -> GitReport {
        self.git_repos_impl(include_submodules, None)
    }

    /// 캐시를 쓰는 버전 — db_path의 git_cache 테이블을 TTL/signature로 조회.
    /// 변경 없는 repo는 git 프로세스를 아예 띄우지 않는다.
    pub fn git_repos_cached(&self, include_submodules: bool, db_path: String) -> GitReport {
        self.git_repos_impl(include_submodules, Some(db_path))
    }
}

impl ScanHandle {
    fn git_repos_impl(&self, include_submodules: bool, db_path: Option<String>) -> GitReport {
        let mut candidates = Vec::new();
        {
            let root = self.root.read().unwrap();
            collect_git_candidates(&root, &self.root_path, &mut candidates);
        }
        // (path, tree_sig) → filter_repos는 경로만 받으므로 매핑 유지.
        let paths: Vec<PathBuf> = candidates.iter().map(|(p, _)| p.clone()).collect();
        let repos = space_git::filter_repos(&paths, include_submodules);
        let tree_sig_of: std::collections::HashMap<PathBuf, u64> = candidates.into_iter().collect();

        const TTL: u64 = 24 * 3600;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let conn = db_path
            .as_ref()
            .and_then(|p| space_index::open(Path::new(p)).ok());
        if let Some(c) = &conn {
            let _ = space_index::git_cache_open(c);
        }

        // 캐시 히트/미스 분리.
        let mut cached: Vec<GitRepoInfo> = Vec::new();
        let mut to_probe: Vec<PathBuf> = Vec::new();
        for repo in &repos {
            let key = repo.to_string_lossy().into_owned();
            let tsig = tree_sig_of.get(repo).copied().unwrap_or(0);
            let gsig = space_git::git_signature(repo);
            let hit = conn.as_ref().and_then(|c| {
                space_index::git_cache_get(c, &key, gsig, tsig, TTL, now)
                    .and_then(|json| repo_info_from_json(&key, &json))
            });
            match hit {
                Some(info) => cached.push(info),
                None => to_probe.push(repo.clone()),
            }
        }

        let (healthy, failures) = space_git::probe_all(&to_probe);

        // 새로 조회한 것 캐시에 저장.
        if let Some(c) = &conn {
            for h in &healthy {
                let key = h.path.to_string_lossy().into_owned();
                let tsig = tree_sig_of.get(&h.path).copied().unwrap_or(0);
                let gsig = space_git::git_signature(&h.path);
                let info = health_to_info(h);
                let _ =
                    space_index::git_cache_put(c, &key, gsig, tsig, &repo_info_to_json(&info), now);
            }
        }

        let mut repos: Vec<GitRepoInfo> = cached
            .into_iter()
            .chain(healthy.iter().map(health_to_info))
            .collect();
        // 위험도순 재정렬 (캐시+신규 혼합).
        repos.sort_by_key(|r| match r.risk.as_str() {
            "danger" => 0u8,
            "caution" => 1,
            "abandoned" => 2,
            "active" => 3,
            _ => 4,
        });
        let failures = failures
            .into_iter()
            .map(|(p, e)| GitProbeFailure {
                path: p.to_string_lossy().into_owned(),
                reason: reason_str(&e).to_string(),
            })
            .collect();
        GitReport { repos, failures }
    }
}

fn describe(h: &space_git::RepoHealth) -> (String, u64, bool) {
    use space_git::{HeadState, UpstreamState};
    let head = match &h.head {
        HeadState::Branch(n) => format!("branch:{}", n),
        HeadState::Detached => "detached".to_string(),
        HeadState::Unborn => "unborn".to_string(),
    };
    let (ahead, no_upstream) = match &h.upstream {
        UpstreamState::Ahead(n) => (*n, false),
        UpstreamState::UpToDate => (0, false),
        UpstreamState::NoUpstream => (0, true),
    };
    (head, ahead, no_upstream)
}

/// 특정 repo의 최근 weeks주 주별 커밋 수 (스파크라인). 지연 조회 — 목록엔 안 넣는다.
#[uniffi::export]
pub fn git_activity(repo_path: String, weeks: u32) -> Vec<u64> {
    space_git::activity(std::path::Path::new(&repo_path), weeks as u64)
}
