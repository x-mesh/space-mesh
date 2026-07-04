//! 실제 홈 디렉토리에서 룰 탐지 실측: cargo run --example detect --release
use std::path::PathBuf;

fn main() {
    let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
    let started = std::time::Instant::now();
    let candidates = space_rules::detect(&home);
    println!(
        "{} candidates in {:.2}s",
        candidates.len(),
        started.elapsed().as_secs_f64()
    );
    for c in candidates.iter().take(12) {
        println!(
            "{:>10.1} MiB  [{}/{}] {} — {}",
            c.allocated_size as f64 / 1048576.0,
            c.rule.category,
            c.rule.safety,
            c.rule.title,
            c.resolved_path.display()
        );
    }
}
