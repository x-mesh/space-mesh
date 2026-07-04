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
                Ok(id) => eprintln!("snapshot #{} saved to {}", id, db_path.display()),
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
