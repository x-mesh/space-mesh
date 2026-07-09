use clap::Parser;
use space_scanner::{scan, top_files, truncate_depth, DirNode, ScanOptions};
use std::path::PathBuf;
use std::time::Instant;

/// space-mesh 스캔 엔진 CLI (M1 검증·벤치용)
#[derive(Parser, Debug)]
#[command(name = "space-mesh", version)]
struct Args {
    /// 스캔할 루트 디렉토리
    path: PathBuf,

    /// 레벨당 표시할 최대 항목 수
    #[arg(long, default_value_t = 10)]
    top: usize,

    /// 디렉토리 트리 표시 깊이
    #[arg(long, default_value_t = 2)]
    depth: usize,

    /// 개별 파일로 기록할 최소 크기 (MiB)
    #[arg(long, default_value_t = 50)]
    min_file_mib: u64,

    /// 루트와 다른 파일시스템으로 내려가지 않음 (du -x 상당)
    #[arg(short = 'x', long)]
    one_filesystem: bool,

    /// 트리를 JSON으로 출력 (depth까지 잘라서)
    #[arg(long)]
    json: bool,

    /// 스캔 결과를 이 SQLite 파일에 스냅샷으로 저장
    #[arg(long)]
    db: Option<PathBuf>,

    /// 스캔 대신 --db의 최근 스냅샷을 로드해 표시
    #[arg(long, requires = "db")]
    from_db: bool,

    /// 최근 두 스냅샷을 비교해 변화의 범인을 출력 (스캔 안 함)
    #[arg(long, requires = "db")]
    diff: bool,

    /// diff에서 보고할 최소 변화량 (MiB)
    #[arg(long, default_value_t = 10)]
    min_delta_mib: u64,

    /// Stale files 섹션에서 방치로 간주할 최소 경과일
    #[arg(long, default_value_t = 180)]
    stale_days: u64,

    /// 트리를 ncdu -f 호환 JSON으로 출력 (전체 깊이; 임계 미만 파일은 잔여 의사항목)
    #[arg(long, conflicts_with = "json")]
    ncdu: bool,

    /// 중복 파일 그룹 출력 (크기→부분해시→전체해시, 첫 파일 = 보존 추천본)
    #[arg(long)]
    dups: bool,

    /// --dups에서 검사할 최소 파일 크기 (MiB)
    #[arg(long, default_value_t = 10)]
    min_dup_mib: u64,

    /// 스캔 후 빌드 산출물 카테고리(node_modules, target 등) 히트 출력
    #[arg(long)]
    categories: bool,

    /// 설치된 도구의 공식 정리 커맨드 제안 출력 (스캔 없음, dry-run 실행)
    #[arg(long)]
    advice: bool,
}

fn main() {
    // head 등으로 stdout 파이프가 먼저 닫혀도 panic 대신 조용히 종료.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args = Args::parse();
    let opts = ScanOptions {
        record_file_threshold: args.min_file_mib * 1024 * 1024,
        one_filesystem: args.one_filesystem,
    };

    if args.advice {
        run_advice(&args);
        return;
    }
    if args.dups {
        run_dups(&args);
        return;
    }
    if args.diff {
        run_diff(&args);
        return;
    }

    let started = Instant::now();
    let (mut root, total_files, total_dirs, errors, source) = if args.from_db {
        let db_path = args.db.as_ref().unwrap();
        let conn = space_index::open(db_path).unwrap_or_else(|e| {
            eprintln!("error: open db {}: {}", db_path.display(), e);
            std::process::exit(1);
        });
        match space_index::load_latest(&conn, &args.path) {
            Ok(Some((meta, root))) => {
                let src = format!("snapshot #{} ({})", meta.scan_id, meta.created_at);
                (root, meta.total_files, meta.total_dirs, 0, src)
            }
            Ok(None) => {
                eprintln!(
                    "error: no snapshot for {} in {}",
                    args.path.display(),
                    db_path.display()
                );
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("error: load snapshot: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        let result = match scan(&args.path, opts) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: {}: {}", args.path.display(), e);
                std::process::exit(1);
            }
        };
        if let Some(db_path) = &args.db {
            let mut conn = space_index::open(db_path).unwrap_or_else(|e| {
                eprintln!("error: open db {}: {}", db_path.display(), e);
                std::process::exit(1);
            });
            match space_index::save_snapshot(&mut conn, &args.path, &result) {
                Ok(id) => {
                    eprintln!("snapshot #{} saved to {}", id, db_path.display());
                    // 루트당 최근 N개만 유지 (PERF-005).
                    match space_index::prune_snapshots(
                        &mut conn,
                        &args.path,
                        space_index::DEFAULT_KEEP_SNAPSHOTS,
                    ) {
                        Ok(n) if n > 0 => eprintln!("pruned {} old snapshot(s)", n),
                        _ => {}
                    }
                }
                Err(e) => eprintln!("warning: snapshot save failed: {}", e),
            }
        }
        (
            result.root,
            result.stats.total_files,
            result.stats.total_dirs,
            result.stats.errors,
            "live scan".to_string(),
        )
    };
    let elapsed = started.elapsed();

    if args.categories {
        run_categories(&root, &args, elapsed);
        return;
    }
    if args.ncdu {
        print_ncdu(&root, &args);
        return;
    }

    let now_epoch = now_epoch();
    // stale은 전체 트리에서 수집해야 하므로 truncate_depth 전에 계산한다.
    let stale = space_scanner::stale_files(&root, args.top, args.stale_days, now_epoch);

    if args.json {
        let stale_json: Vec<_> = stale
            .iter()
            .map(|f| {
                serde_json::json!({
                    "path": f.path,
                    "allocated_size": f.allocated_size,
                    "modified_epoch": f.modified_epoch,
                    "age_days": space_scanner::age_days(f.modified_epoch, now_epoch),
                })
            })
            .collect();
        truncate_depth(&mut root, args.depth);
        let out = serde_json::json!({
            "root": root,
            "stale": stale_json,
            "stale_days": args.stale_days,
            "stats": {
                "files": total_files,
                "dirs": total_dirs,
                "errors": errors,
                "elapsed_ms": elapsed.as_millis(),
                "source": source,
            },
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return;
    }

    println!(
        "{} files / {} dirs in {:.2}s (skipped on error: {}) [{}]",
        total_files,
        total_dirs,
        elapsed.as_secs_f64(),
        errors,
        source
    );
    println!(
        "Total: logical {}, allocated {}\n",
        human(root.logical_size),
        human(root.allocated_size)
    );

    println!("Directories (allocated, depth {}):", args.depth);
    print_tree(&root, 0, args.depth, args.top, root.allocated_size);

    let files = top_files(&root, args.top);
    if !files.is_empty() {
        println!("\nTop files (>= {} MiB):", args.min_file_mib);
        for f in files {
            println!(
                "  {:>10}  {:>5}  {}",
                human(f.allocated_size),
                age_label(f.modified_epoch, now_epoch),
                f.path.display()
            );
        }
    }

    if !stale.is_empty() {
        println!(
            "\nStale files (>= {} MiB, {}일+ 방치, 크기×방치일 순):",
            args.min_file_mib, args.stale_days
        );
        for f in stale {
            println!(
                "  {:>10}  {:>5}  {}",
                human(f.allocated_size),
                age_label(f.modified_epoch, now_epoch),
                f.path.display()
            );
        }
    }
}

/// ncdu export format 1.0 — `ncdu -f <file>`로 브라우징 가능.
/// 트리에는 임계 이상 파일(big_files)만 개별 기록되므로, 나머지는
/// 디렉토리당 "(임계 미만 파일들)" 의사 항목으로 합산해 총량을 보존한다.
fn print_ncdu(root: &DirNode, args: &Args) {
    let header = serde_json::json!({
        "progname": "space-mesh",
        "progver": env!("CARGO_PKG_VERSION"),
        "timestamp": now_epoch(),
    });
    let mut tree = ncdu_node(root, args.min_file_mib);
    // 루트 이름은 ncdu 표시용 전체 경로로 교체.
    if let Some(entries) = tree.as_array_mut() {
        if let Some(head) = entries.first_mut() {
            head["name"] = serde_json::Value::String(args.path.to_string_lossy().into_owned());
        }
    }
    let out = serde_json::json!([1, 0, header, tree]);
    println!("{}", out);
}

fn ncdu_node(node: &DirNode, min_file_mib: u64) -> serde_json::Value {
    let mut arr: Vec<serde_json::Value> =
        Vec::with_capacity(1 + node.children.len() + node.big_files.len() + 1);
    arr.push(serde_json::json!({ "name": node.name }));

    let mut covered_alloc = 0u64;
    let mut covered_logical = 0u64;
    for c in &node.children {
        covered_alloc += c.allocated_size;
        covered_logical += c.logical_size;
        arr.push(ncdu_node(c, min_file_mib));
    }
    for f in &node.big_files {
        covered_alloc += f.allocated_size;
        covered_logical += f.logical_size;
        let name = f
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| f.path.to_string_lossy().into_owned());
        arr.push(serde_json::json!({
            "name": name,
            "asize": f.logical_size,
            "dsize": f.allocated_size,
        }));
    }
    // 개별 기록되지 않은 파일들 + 디렉토리 자체 블록의 합 (총량 보존).
    let rest_alloc = node.allocated_size.saturating_sub(covered_alloc);
    let rest_logical = node.logical_size.saturating_sub(covered_logical);
    if rest_alloc > 0 || rest_logical > 0 {
        arr.push(serde_json::json!({
            "name": format!("({} MiB 미만 파일들)", min_file_mib),
            "asize": rest_logical,
            "dsize": rest_alloc,
        }));
    }
    serde_json::Value::Array(arr)
}

/// 중복 파일 그룹 — 첫 파일 = 보존 추천본(최신 mtime).
fn run_dups(args: &Args) {
    let started = Instant::now();
    let result =
        space_dedup::find_duplicates(&args.path, args.min_dup_mib.max(1) * 1024 * 1024, None)
            .unwrap_or_else(|e| {
                eprintln!("error: {}: {}", args.path.display(), e);
                std::process::exit(1);
            });
    let elapsed = started.elapsed();
    let total_reclaimable: u64 = result.groups.iter().map(|g| g.reclaimable).sum();

    if args.json {
        let out = serde_json::json!({
            "min_dup_mib": args.min_dup_mib,
            "total_reclaimable": total_reclaimable,
            "stats": {
                "candidates": result.stats.candidates,
                "partial_hashed": result.stats.partial_hashed,
                "full_hashed": result.stats.full_hashed,
                "elapsed_ms": elapsed.as_millis(),
            },
            "groups": result.groups.iter().map(|g| serde_json::json!({
                "file_size": g.file_size,
                "reclaimable": g.reclaimable,
                "hash": g.hash_hex,
                // 첫 파일 = 보존 추천본 (최신 mtime, 동률은 경로순).
                "files": g.files.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return;
    }

    println!(
        "{} groups, 회수 가능 {} (>= {} MiB, {:.2}s, 후보 {} → 부분해시 {} → 전체해시 {})",
        result.groups.len(),
        human(total_reclaimable),
        args.min_dup_mib,
        elapsed.as_secs_f64(),
        result.stats.candidates,
        result.stats.partial_hashed,
        result.stats.full_hashed
    );
    for g in result.groups.iter().take(args.top.max(20)) {
        println!(
            "\n{} × {}  회수 {}  [{}]",
            g.files.len(),
            human(g.file_size),
            human(g.reclaimable),
            &g.hash_hex[..12.min(g.hash_hex.len())]
        );
        for (i, f) in g.files.iter().enumerate() {
            let tag = if i == 0 { "  KEEP " } else { "       " };
            println!("{}{}", tag, f.display());
        }
    }
}

/// 스캔 트리에서 빌드 산출물 카테고리 히트 출력.
fn run_categories(root: &DirNode, args: &Args, elapsed: std::time::Duration) {
    let hits = space_rules::categories::find_categories(root, &args.path);
    let idle = space_rules::categories::annotate_idle(&hits);
    let total: u64 = hits.iter().map(|h| h.allocated_size).sum();

    if args.json {
        let out = serde_json::json!({
            "total_reclaimable": total,
            "elapsed_ms": elapsed.as_millis(),
            "hits": hits.iter().map(|h| serde_json::json!({
                "category": h.def.id,
                "title": h.def.title,
                "safety": h.def.safety,
                "path": h.path.to_string_lossy(),
                "project": h.project_path.to_string_lossy(),
                "allocated_size": h.allocated_size,
                "file_count": h.file_count,
                "verified": h.verified,
                "recreate_command": h.def.recreate_command,
                "idle_days": idle.get(&h.project_path),
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return;
    }

    println!(
        "{} hits, 합계 {} ({:.2}s)",
        hits.len(),
        human(total),
        elapsed.as_secs_f64()
    );
    for h in hits.iter().take(args.top.max(20)) {
        let idle_label = idle
            .get(&h.project_path)
            .map(|d| format!("  idle {}d", d))
            .unwrap_or_default();
        println!(
            "  {:>10}  [{}] {}{}{}",
            human(h.allocated_size),
            h.def.safety,
            h.path.display(),
            if h.verified { "" } else { "  (unverified)" },
            idle_label
        );
    }
}

/// 설치된 도구의 공식 정리 커맨드 제안 (brew/docker 등 dry-run).
fn run_advice(args: &Args) {
    let advices = space_rules::advisor::advise();
    if args.json {
        let out = serde_json::json!({
            "advices": advices.iter().map(|a| serde_json::json!({
                "tool": a.tool,
                "command": a.command,
                "description": a.description,
                "estimated_reclaim": a.estimated_reclaim,
                "available": a.available,
                "detail": a.detail,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return;
    }
    for a in &advices {
        let reclaim = a
            .estimated_reclaim
            .map(|r| format!("  예상 회수 {}", human(r)))
            .unwrap_or_default();
        println!(
            "  [{}] {}{}\n      $ {}\n      {} — {}",
            if a.available { "ok" } else { "--" },
            a.tool,
            reclaim,
            a.command,
            a.description,
            a.detail
        );
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "382d" 같은 경과일 라벨. mtime을 모르면 "-".
fn age_label(modified_epoch: i64, now_epoch: i64) -> String {
    if modified_epoch <= 0 {
        return "-".to_string();
    }
    format!(
        "{}d",
        space_scanner::age_days(modified_epoch, now_epoch).max(0)
    )
}

/// 최근 두 스냅샷 비교 — 변화의 범인 출력.
fn run_diff(args: &Args) {
    let db_path = args.db.as_ref().unwrap();
    let conn = space_index::open(db_path).unwrap_or_else(|e| {
        eprintln!("error: open db {}: {}", db_path.display(), e);
        std::process::exit(1);
    });
    let snaps = space_index::list_snapshots(&conn, &args.path).unwrap_or_else(|e| {
        eprintln!("error: list snapshots: {}", e);
        std::process::exit(1);
    });
    if snaps.len() < 2 {
        eprintln!(
            "error: {}의 스냅샷이 {}개 — diff에는 2개 이상 필요 (--db로 스캔을 두 번 실행)",
            args.path.display(),
            snaps.len()
        );
        std::process::exit(1);
    }
    let (new, old) = (&snaps[0], &snaps[1]);
    let entries = space_index::diff_snapshots(
        &conn,
        old.scan_id,
        new.scan_id,
        args.min_delta_mib * 1024 * 1024,
    )
    .unwrap_or_else(|e| {
        eprintln!("error: diff: {}", e);
        std::process::exit(1);
    });

    if args.json {
        let out = serde_json::json!({
            "old": {"scan_id": old.scan_id, "created_at": old.created_at},
            "new": {"scan_id": new.scan_id, "created_at": new.created_at},
            "min_delta_mib": args.min_delta_mib,
            "entries": entries.iter().map(|e| serde_json::json!({
                "path": e.path,
                "delta": e.delta,
                "before_total": e.before_total,
                "after_total": e.after_total,
                "is_residual": e.is_residual,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return;
    }

    let total_delta: i64 = entries.iter().map(|e| e.delta).sum();
    println!(
        "snapshot #{} ({}) → #{} ({}), 항목 {}개, 순변화 {}{}",
        old.scan_id,
        old.created_at,
        new.scan_id,
        new.created_at,
        entries.len(),
        if total_delta >= 0 { "+" } else { "-" },
        human(total_delta.unsigned_abs())
    );
    for e in entries.iter().take(args.top.max(20)) {
        println!(
            "{:>12}  {}{}",
            format!(
                "{}{}",
                if e.delta >= 0 { "+" } else { "-" },
                human(e.delta.unsigned_abs())
            ),
            e.path,
            if e.is_residual { "  (직속)" } else { "" }
        );
    }
}

fn print_tree(node: &DirNode, depth: usize, max_depth: usize, top: usize, total: u64) {
    let indent = "  ".repeat(depth);
    let pct = if total > 0 {
        node.allocated_size as f64 / total as f64 * 100.0
    } else {
        0.0
    };
    println!(
        "{}{:>10}  {:>5.1}%  {}",
        indent,
        human(node.allocated_size),
        pct,
        node.name
    );
    if depth >= max_depth {
        return;
    }
    let mut children: Vec<&DirNode> = node.children.iter().collect();
    children.sort_by_key(|f| std::cmp::Reverse(f.allocated_size));
    for c in children.into_iter().take(top) {
        print_tree(c, depth + 1, max_depth, top, total);
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, UNITS[i])
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}
