//! 실측: cargo run --example categories --release -- <root>
use space_rules::categories::find_categories;
use space_scanner::{scan, ScanOptions};
use std::collections::HashMap;
use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| std::env::var("HOME").unwrap()),
    );
    let t0 = std::time::Instant::now();
    let result = scan(&root, ScanOptions::default()).expect("scan");
    let t1 = std::time::Instant::now();
    let hits = find_categories(&result.root, &root);
    let t2 = std::time::Instant::now();

    let mut by_cat: HashMap<&str, (usize, u64)> = HashMap::new();
    for h in &hits {
        let e = by_cat.entry(h.def.id).or_default();
        e.0 += 1;
        e.1 += h.allocated_size;
    }
    let mut rows: Vec<_> = by_cat.into_iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1 .1));

    println!(
        "scan {:.1}s + categorize {:.3}s — {} hits",
        (t1 - t0).as_secs_f64(),
        (t2 - t1).as_secs_f64(),
        hits.len()
    );
    for (id, (count, size)) in rows {
        println!(
            "{:>10.1} MiB  {:>4}곳  {}",
            size as f64 / 1048576.0,
            count,
            id
        );
    }
    let unverified = hits.iter().filter(|h| !h.verified).count();
    println!("unverified: {}", unverified);
}
