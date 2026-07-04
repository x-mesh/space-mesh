use std::path::PathBuf;
fn find_git_dirs(root: &std::path::Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 6 { return; }
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let mut has_git = false;
    let mut subdirs = vec![];
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() { continue; }
        let name = e.file_name();
        let n = name.to_string_lossy();
        if n == ".git" { has_git = true; continue; }
        if n == "node_modules" || n == "target" || n == ".build" { continue; }
        subdirs.push(p);
    }
    if has_git { out.push(root.to_path_buf()); }
    for s in subdirs { find_git_dirs(&s, out, depth+1); }
}
fn main() {
    let root = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| std::env::var("HOME").unwrap()));
    let t0 = std::time::Instant::now();
    let mut cands = vec![];
    find_git_dirs(&root, &mut cands, 0);
    let repos = space_git::filter_repos(&cands, false);
    let (ok, fail) = space_git::probe_all(&repos);
    let t = t0.elapsed();
    println!("발견 {} candidates → {} repos, {}개 조회성공 {}개 실패, {:.2}s", cands.len(), repos.len(), ok.len(), fail.len(), t.as_secs_f64());
    let mut by_risk = std::collections::HashMap::new();
    for r in &ok { *by_risk.entry(format!("{:?}", r.risk)).or_insert(0) += 1; }
    println!("위험도 분포: {:?}", by_risk);
    for r in ok.iter().take(8) {
        println!("  [{:?}] {} dirty={} untracked={} stash={} ahead={:?} remote={} {}일전",
            r.risk, r.path.file_name().unwrap().to_string_lossy(),
            r.tracked_dirty, r.untracked_present, r.stash_count, r.upstream, r.has_remote,
            r.last_commit_days.map(|d| d as i64).unwrap_or(-1));
    }
    if !fail.is_empty() { println!("실패: {:?}", fail.iter().map(|(_,e)| format!("{:?}",e)).collect::<Vec<_>>()); }
}
