//! M1 스파이크: 증분 병합의 트리 복제 비용 실측.
//!
//! 사용: cargo run --release -p space-scanner --example clone_bench -- <root>
//! 측정: ①DirNode 전체 deep clone ②Arc::try_unwrap(참조 1) 회수 ③참조 2일 때 폴백(clone)

use std::sync::Arc;
use std::time::Instant;

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: clone_bench <root>");
        std::process::exit(2);
    });
    let started = Instant::now();
    let result = space_scanner::scan(root.as_ref(), space_scanner::ScanOptions::default())
        .expect("scan failed");
    println!(
        "scan: {} files / {} dirs in {:.2}s",
        result.stats.total_files,
        result.stats.total_dirs,
        started.elapsed().as_secs_f64()
    );

    // ① deep clone ×3
    for i in 1..=3 {
        let t = Instant::now();
        let cloned = result.root.clone();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("clone #{i}: {:.1}ms (dirs={})", ms, cloned.dir_count);
    }

    // ② Arc::try_unwrap — 참조 1이면 무복제 회수 (이동만).
    let arc = Arc::new(result.root);
    let t = Instant::now();
    let owned = Arc::try_unwrap(arc).expect("refcount must be 1");
    println!(
        "try_unwrap(refs=1): {:.3}ms (이동만 — 무복제)",
        t.elapsed().as_secs_f64() * 1000.0
    );

    // ③ 참조 2일 때: try_unwrap 실패 → clone 폴백 경로 비용.
    let arc = Arc::new(owned);
    let holder = Arc::clone(&arc);
    let t = Instant::now();
    let recovered = match Arc::try_unwrap(arc) {
        Ok(n) => n,
        Err(shared) => (*shared).clone(),
    };
    println!(
        "try_unwrap 실패 → clone 폴백: {:.1}ms",
        t.elapsed().as_secs_f64() * 1000.0
    );
    drop(holder);
    drop(recovered);
}
