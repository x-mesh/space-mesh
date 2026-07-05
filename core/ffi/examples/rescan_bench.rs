//! M5 검증: rescan_paths E2E 실측 (R1 ≤1s, R2 총량 동등).
//!
//! 사용: cargo run --release -p space-ffi --example rescan_bench -- <root> <db> <dir1> [dir2...]
//! 무변경 재스캔도 clone + 서브트리 스캔 + 병합 + 전체 스냅샷 저장 + 프루닝의
//! 동일 비용 경로를 태운다 — R1의 지배 항(저장)을 포함한 정직한 E2E.

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: rescan_bench <root> <db> <dir...>");
        std::process::exit(2);
    }
    let root = args[1].clone();
    let db = args[2].clone();
    let targets: Vec<String> = args[3..].to_vec();

    let t0 = Instant::now();
    let handle = space_ffi::scan_path(root.clone(), 10).expect("full scan failed");
    let full_secs = t0.elapsed().as_secs_f64();
    let s = handle.stats();
    println!(
        "풀스캔: {} files / {} dirs in {:.2}s",
        s.total_files, s.total_dirs, full_secs
    );

    // R1: 증분 재집계 (재스캔 + 병합 + 스냅샷 저장 + 프루닝) ×3
    for i in 1..=3 {
        let t = Instant::now();
        let report = handle
            .rescan_paths(targets.clone(), 10, db.clone())
            .expect("rescan failed");
        let secs = t.elapsed().as_secs_f64();
        let rs = report.handle.stats();
        println!(
            "증분 #{i}: {:.3}s (degraded={}, 서브트리 {}개, files={})",
            secs, report.degraded, report.rescanned_dirs, rs.total_files
        );
        if i == 1 {
            // R2: 무변경 증분 결과 총량 == 원본 핸들 총량 (파일/디렉토리 수).
            assert_eq!(rs.total_files, s.total_files, "R2 위반: 파일 수 불일치");
            assert_eq!(rs.total_dirs, s.total_dirs, "R2 위반: 디렉토리 수 불일치");
            let orig_alloc = handle.node_at(vec![]).unwrap().allocated_size;
            let new_alloc = report.handle.node_at(vec![]).unwrap().allocated_size;
            assert_eq!(new_alloc, orig_alloc, "R2 위반: allocated 불일치");
            println!("R2 총량 동등: OK (allocated={} 일치)", new_alloc);
        }
    }
}
