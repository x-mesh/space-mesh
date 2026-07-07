use clap::Parser;
use space_scanner::{scan, top_files, truncate_depth, DirNode, ScanOptions};
use std::path::PathBuf;
use std::time::Instant;

/// space-mesh 스캔 엔진 CLI (M1 검증·벤치용)
#[derive(Parser, Debug)]
#[command(name = "space-mesh", version)]
struct Args {
    /// 스캔할 루트 디렉토리 (--detect/--advise는 경로가 필요 없다)
    #[arg(required_unless_present_any = ["detect", "advise"])]
    path: Option<PathBuf>,

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

    /// 내장 룰셋으로 홈 디렉토리의 정리 후보를 탐지 (스캔 안 함)
    #[arg(long)]
    detect: bool,

    /// path 아래 중복 파일 그룹 탐지 (--min-file-mib 이상)
    #[arg(long)]
    dups: bool,

    /// 설치된 도구의 공식 정리 커맨드 제안 (dry-run 예상치 포함, 스캔 안 함)
    #[arg(long)]
    advise: bool,

    /// 스캔 후 정책 기반 회수 제안 생성: safe 룰 후보 + 유휴 프로젝트의 safe 빌드 산출물
    #[arg(long)]
    suggest: bool,

    /// --suggest 결과 JSON을 쓸 파일 (생략 시 stdout으로 출력하고 종료)
    #[arg(long, requires = "suggest")]
    suggest_out: Option<PathBuf>,

    /// --suggest: git 마지막 커밋이 이 일수 이상인 프로젝트의 산출물만
    #[arg(long, default_value_t = 90)]
    idle_days: u64,

    /// --suggest: 제안 합계가 이 값(MiB) 미만이면 항목을 비운다 (소음 방지)
    #[arg(long, default_value_t = 512)]
    suggest_min_mib: u64,
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

    if args.diff {
        run_diff(&args);
        return;
    }
    if args.detect {
        run_detect(&args);
        return;
    }
    if args.dups {
        run_dups(&args);
        return;
    }
    if args.advise {
        run_advise(&args);
        return;
    }

    let started = Instant::now();
    let (mut root, total_files, total_dirs, errors, source) = if args.from_db {
        let db_path = args.db.as_ref().unwrap();
        let conn = space_index::open(db_path).unwrap_or_else(|e| {
            eprintln!("error: open db {}: {}", db_path.display(), e);
            std::process::exit(1);
        });
        match space_index::load_latest(&conn, args.scan_root()) {
            Ok(Some((meta, root))) => {
                let src = format!("snapshot #{} ({})", meta.scan_id, meta.created_at);
                (root, meta.total_files, meta.total_dirs, 0, src)
            }
            Ok(None) => {
                eprintln!(
                    "error: no snapshot for {} in {}",
                    args.scan_root().display(),
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
        let result = match scan(args.scan_root(), opts) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: {}: {}", args.scan_root().display(), e);
                std::process::exit(1);
            }
        };
        if let Some(db_path) = &args.db {
            let mut conn = space_index::open(db_path).unwrap_or_else(|e| {
                eprintln!("error: open db {}: {}", db_path.display(), e);
                std::process::exit(1);
            });
            match space_index::save_snapshot(&mut conn, args.scan_root(), &result) {
                Ok(id) => eprintln!("snapshot #{} saved to {}", id, db_path.display()),
                Err(e) => eprintln!("warning: snapshot save failed: {}", e),
            }
            // 보존 정책 (F7): 7일 전체 → 30일 일별 → 이후 주별. 주기 모드 무한 축적 방지.
            if let Ok(pruned) = space_index::prune_snapshots(&mut conn) {
                if pruned > 0 {
                    eprintln!("pruned {} old snapshot(s)", pruned);
                }
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

    // 정책 기반 회수 제안 (F6/F8) — 방금 스캔한 트리를 재사용한다.
    if args.suggest {
        let payload = build_suggestions(&args, &root);
        match &args.suggest_out {
            Some(out) => {
                match std::fs::write(out, serde_json::to_string_pretty(&payload).unwrap()) {
                    Ok(()) => eprintln!("suggestions written to {}", out.display()),
                    Err(e) => eprintln!("warning: suggest write failed: {}", e),
                }
            }
            None => {
                println!("{}", serde_json::to_string_pretty(&payload).unwrap());
                return;
            }
        }
    }

    if args.json {
        truncate_depth(&mut root, args.depth);
        let out = serde_json::json!({
            "root": root,
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
            println!("  {:>10}  {}", human(f.allocated_size), f.path.display());
        }
    }
}

impl Args {
    /// detect/advise 외 모드에서는 clap의 required_unless가 존재를 보장한다.
    fn scan_root(&self) -> &PathBuf {
        self.path
            .as_ref()
            .expect("clap enforces path for this mode")
    }
}

fn home_dir(args: &Args) -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| args.path.clone())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 내장 룰셋으로 홈의 정리 후보 탐지 (F8). --json 스키마는 FFI Record와 필드명 일치.
fn run_detect(args: &Args) {
    let candidates = space_rules::detect(&home_dir(args));
    if args.json {
        let out = serde_json::json!({
            "candidates": candidates.iter().map(|c| serde_json::json!({
                "rule_id": c.rule.id,
                "title": c.rule.title,
                "category": c.rule.category,
                "safety": c.rule.safety,
                "path": c.resolved_path,
                "allocated_size": c.allocated_size,
                "file_count": c.file_count,
                "recreate_command": c.rule.recreate_command,
                "recreate_cost": c.rule.recreate_cost,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return;
    }
    println!("Cleanup candidates ({}):", candidates.len());
    for c in &candidates {
        println!(
            "  {:>10}  [{}] {}  ({})",
            human(c.allocated_size),
            c.rule.safety,
            c.rule.title,
            c.resolved_path.display()
        );
    }
}

/// path 아래 중복 파일 그룹 (F8). 클론 공유 보정 포함.
fn run_dups(args: &Args) {
    let result = space_dedup::find_duplicates(
        args.scan_root(),
        args.min_file_mib.max(1) * 1024 * 1024,
        None,
    )
    .unwrap_or_else(|e| {
        eprintln!("error: dups: {}", e);
        std::process::exit(1);
    });
    if args.json {
        let out = serde_json::json!({
            "groups": result.groups.iter().map(|g| serde_json::json!({
                "file_size": g.file_size,
                "reclaimable": g.reclaimable,
                "clone_shared": g.clone_shared,
                "hash_hex": g.hash_hex,
                "files": g.files,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return;
    }
    println!("Duplicate groups ({}):", result.groups.len());
    for g in &result.groups {
        println!(
            "  {} × {:>10}  reclaimable {}{}",
            g.files.len(),
            human(g.file_size),
            human(g.reclaimable),
            if g.clone_shared {
                "  (clone-shared)"
            } else {
                ""
            }
        );
        for f in &g.files {
            println!("      {}", f.display());
        }
    }
}

/// 설치된 도구의 공식 정리 커맨드 제안 (F8).
fn run_advise(args: &Args) {
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
        println!(
            "  {}{}  {}  — {}",
            a.tool,
            if a.available { "" } else { " (없음)" },
            a.command,
            a.estimated_reclaim.map(human).unwrap_or_default()
        );
    }
}

/// 정책 평가 (F6): safe 룰 후보 전부 + 유휴 프로젝트의 verified safe 빌드 산출물.
/// 합계가 suggest_min_mib 미만이면 항목을 비워 소음을 막는다 (below_threshold로 표시).
fn build_suggestions(args: &Args, root: &DirNode) -> serde_json::Value {
    let mut items: Vec<serde_json::Value> = Vec::new();
    let mut total: u64 = 0;

    for c in space_rules::detect(&home_dir(args)) {
        if c.rule.safety != "safe" {
            continue;
        }
        total += c.allocated_size;
        items.push(serde_json::json!({
            "path": c.resolved_path,
            "title": c.rule.title,
            "source": "rule",
            "safety": c.rule.safety,
            "estimated": c.allocated_size,
            "recreate_command": c.rule.recreate_command,
            "idle_days": serde_json::Value::Null,
        }));
    }

    let hits = space_rules::categories::find_categories(root, args.scan_root());
    let idle = space_rules::categories::annotate_idle(&hits);
    for h in &hits {
        let days = idle.get(&h.project_path).copied();
        if !h.verified || h.def.safety != "safe" || days.unwrap_or(0) < args.idle_days {
            continue;
        }
        total += h.allocated_size;
        items.push(serde_json::json!({
            "path": h.path,
            "title": h.def.title,
            "source": "category",
            "safety": h.def.safety,
            "estimated": h.allocated_size,
            "recreate_command": h.def.recreate_command,
            "idle_days": days,
        }));
    }

    let below = total < args.suggest_min_mib * 1024 * 1024;
    let generated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    serde_json::json!({
        "version": 1,
        "generated_at": generated_at,
        "root": args.scan_root(),
        "idle_days": args.idle_days,
        "total_estimated": total,
        "below_threshold": below,
        "items": if below { Vec::new() } else { items },
    })
}

/// 최근 두 스냅샷 비교 — 변화의 범인 출력.
fn run_diff(args: &Args) {
    let db_path = args.db.as_ref().unwrap();
    let conn = space_index::open(db_path).unwrap_or_else(|e| {
        eprintln!("error: open db {}: {}", db_path.display(), e);
        std::process::exit(1);
    });
    let snaps = space_index::list_snapshots(&conn, args.scan_root()).unwrap_or_else(|e| {
        eprintln!("error: list snapshots: {}", e);
        std::process::exit(1);
    });
    if snaps.len() < 2 {
        eprintln!(
            "error: {}의 스냅샷이 {}개 — diff에는 2개 이상 필요 (--db로 스캔을 두 번 실행)",
            args.scan_root().display(),
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
    children.sort_by(|a, b| b.allocated_size.cmp(&a.allocated_size));
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
