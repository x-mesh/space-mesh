//! 정책 기반 회수 제안 (F6) — 단일 구현.
//!
//! CLI `--suggest`(suggestions.json)와 앱 FFI(ScanHandle.suggestions)가 모두
//! 이 함수를 쓴다. 정책이 두 언어에 이중 구현되면 주기 모드와 live 모드가
//! 같은 디스크에 다른 제안을 내므로, 필터와 스키마는 여기 한 곳에만 둔다.
//!
//! 정책: safe 룰 후보 전부 + 유휴(idle_days 이상) 프로젝트의 verified safe
//! 빌드 산출물. git이 없어 유휴 판정이 불가능한 프로젝트는 보수적으로 제외.
//! 합계가 min_bytes 미만이면 항목을 비운다 (소음 방지, below_threshold 표시).

use crate::categories;
use serde::Serialize;
use space_scanner::DirNode;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct SuggestionItem {
    /// 화면에 보여줄 대표 위치.
    pub path: PathBuf,
    /// 실제로 휴지통에 보낼 경로들 — `path`와 다를 수 있다. 룰 후보는 다른 룰이
    /// 안쪽을 따로 잡고 있으면 그 자식들이 빠진 나머지가 들어온다.
    /// **실행은 반드시 이걸 써야 한다** (Candidate::delete_paths와 같은 계약).
    pub delete_paths: Vec<PathBuf>,
    pub title: String,
    /// "rule" | "category"
    pub source: String,
    pub safety: String,
    pub estimated: u64,
    pub recreate_command: String,
    pub idle_days: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Suggestion {
    pub version: u32,
    /// unix 초.
    pub generated_at: u64,
    pub root: PathBuf,
    pub idle_days: u64,
    pub total_estimated: u64,
    pub below_threshold: bool,
    pub items: Vec<SuggestionItem>,
}

/// 스캔 트리와 홈 디렉토리를 평가해 제안을 만든다.
/// 룰 경로 측정(detect)이 가벼운 스캔을 동반하므로 블로킹 — 백그라운드에서 호출.
pub fn build(
    root_tree: &DirNode,
    root_path: &Path,
    home: &Path,
    idle_days: u64,
    min_bytes: u64,
) -> Suggestion {
    let mut items: Vec<SuggestionItem> = Vec::new();
    let mut total: u64 = 0;

    for c in crate::detect(home) {
        if c.rule.safety != "safe" {
            continue;
        }
        total += c.allocated_size;
        items.push(SuggestionItem {
            path: c.resolved_path,
            delete_paths: c.delete_paths,
            title: c.rule.title,
            source: "rule".to_string(),
            safety: c.rule.safety,
            estimated: c.allocated_size,
            recreate_command: c.rule.recreate_command,
            idle_days: None,
        });
    }

    let hits = categories::find_categories(root_tree, root_path);
    let idle = categories::annotate_idle(&hits);
    for h in &hits {
        let days = idle.get(&h.project_path).copied();
        // 유휴 판정이 불가능한(git 없는) 프로젝트는 보수적으로 제외.
        if !h.verified || h.def.safety != "safe" || days.unwrap_or(0) < idle_days {
            continue;
        }
        total += h.allocated_size;
        items.push(SuggestionItem {
            path: h.path.clone(),
            delete_paths: vec![h.path.clone()],
            title: h.def.title.to_string(),
            source: "category".to_string(),
            safety: h.def.safety.to_string(),
            estimated: h.allocated_size,
            recreate_command: h.def.recreate_command.to_string(),
            idle_days: days,
        });
    }

    let below = total < min_bytes;
    Suggestion {
        version: 1,
        generated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        root: root_path.to_path_buf(),
        idle_days,
        total_estimated: total,
        below_threshold: below,
        items: if below { Vec::new() } else { items },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space_scanner::{scan, ScanOptions};
    use std::fs;

    #[test]
    fn suggests_safe_rules_and_respects_threshold() {
        let base = std::env::temp_dir().join(format!("space-suggest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        // 가짜 홈: safe 룰(homebrew-cache) 대상 + warn 룰(휴지통)은 제외돼야 함.
        let brew = base.join("home/Library/Caches/Homebrew");
        fs::create_dir_all(&brew).unwrap();
        fs::write(brew.join("pkg.tar"), vec![0u8; 60_000]).unwrap();
        let scan_root = base.join("scan");
        fs::create_dir_all(&scan_root).unwrap();
        let tree = scan(&scan_root, ScanOptions::default()).unwrap().root;

        let s = build(&tree, &scan_root, &base.join("home"), 90, 0);
        assert!(s.items.iter().any(|i| i.source == "rule"));
        assert!(s.items.iter().all(|i| i.safety == "safe"));
        assert!(!s.below_threshold);
        assert_eq!(
            s.total_estimated,
            s.items.iter().map(|i| i.estimated).sum::<u64>()
        );
        // 실행 대상은 항상 delete_paths — 비어 있으면 지울 게 없다는 뜻이라 안 된다.
        assert!(s.items.iter().all(|i| !i.delete_paths.is_empty()));

        // 임계값 미달이면 items를 비우고 below_threshold로 표시.
        let quiet = build(&tree, &scan_root, &base.join("home"), 90, u64::MAX);
        assert!(quiet.below_threshold);
        assert!(quiet.items.is_empty());

        fs::remove_dir_all(&base).unwrap();
    }
}
