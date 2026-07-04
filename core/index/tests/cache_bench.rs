// 캐시 없음 vs 있음 왕복 검증 (index 크레이트 캐시 API 단위).
use space_index as idx;

#[test]
fn cache_roundtrip_and_invalidation() {
    let db = std::env::temp_dir().join(format!("git-cache-test-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let conn = idx::open(&db).unwrap();
    idx::git_cache_open(&conn).unwrap();

    let now = 1_000_000u64;
    // 저장.
    idx::git_cache_put(&conn, "/a/repo", 111, 222, "danger|branch:main|0|0|1|1|0|0|5|-1", now).unwrap();

    // 같은 sig + TTL 이내 → 히트.
    assert!(idx::git_cache_get(&conn, "/a/repo", 111, 222, 3600, now + 100).is_some());
    // git_sig 변경 → 미스.
    assert!(idx::git_cache_get(&conn, "/a/repo", 999, 222, 3600, now + 100).is_none());
    // tree_sig 변경 → 미스.
    assert!(idx::git_cache_get(&conn, "/a/repo", 111, 999, 3600, now + 100).is_none());
    // TTL 초과 → 미스.
    assert!(idx::git_cache_get(&conn, "/a/repo", 111, 222, 3600, now + 4000).is_none());

    drop(conn);
    let _ = std::fs::remove_file(&db);
}
