//! 안전등급 태깅된 룰셋 기반 불필요 파일 탐지.
//!
//! 룰은 rules.json(빌드에 내장)으로 관리한다 — 스키마는 외부 룰 파일 로드도 지원해
//! 앱 업데이트 없이 갱신 가능한 구조를 유지한다.
//! (계획상 YAML이었으나 serde_yaml 미유지보수로 JSON 채택.)

pub mod advisor;
pub mod categories;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use space_scanner::{scan, ScanOptions};
use std::path::{Path, PathBuf};

pub const EMBEDDED_RULES: &str = include_str!("../rules.json");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub title: String,
    pub category: String,
    /// `~/`로 시작하면 홈 기준 상대 경로.
    pub path: String,
    /// "safe" = 원클릭 정리 가능, "warn" = 검토 필요.
    pub safety: String,
    pub description: String,
    /// 삭제 후 복원 명령 (빈 문자열 = 자동 재생성 또는 해당 없음).
    #[serde(default)]
    pub recreate_command: String,
    /// 재생성 비용: low | medium | high (빈 문자열 = 미지정).
    #[serde(default)]
    pub recreate_cost: String,
}

#[derive(Debug, Deserialize)]
struct RuleFile {
    #[allow(dead_code)]
    version: u32,
    rules: Vec<Rule>,
}

/// 룰 하나가 실제 디스크에서 차지하는 공간을 측정한 결과.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub rule: Rule,
    pub resolved_path: PathBuf,
    pub allocated_size: u64,
    pub file_count: u64,
}

pub fn load_rules() -> Vec<Rule> {
    parse_rules(EMBEDDED_RULES).expect("embedded rules.json must parse")
}

pub fn parse_rules(json: &str) -> Result<Vec<Rule>, serde_json::Error> {
    Ok(serde_json::from_str::<RuleFile>(json)?.rules)
}

fn expand(path: &str, home: &Path) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// 모든 룰의 대상 경로를 병렬로 측정해 존재하고 크기 > 0인 후보만 반환한다.
/// 트리 전체 스캔과 무관하게 룰 경로만 직접 스캔하므로 빠르다.
pub fn detect(home: &Path) -> Vec<Candidate> {
    detect_with_rules(home, &load_rules())
}

pub fn detect_with_rules(home: &Path, rules: &[Rule]) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = rules
        .par_iter()
        .filter_map(|rule| {
            let target = expand(&rule.path, home);
            if !target.is_dir() {
                return None;
            }
            let result = scan(
                &target,
                ScanOptions {
                    record_file_threshold: u64::MAX, // 개별 파일 기록 불필요
                    one_filesystem: false,
                },
            )
            .ok()?;
            if result.root.allocated_size == 0 {
                return None;
            }
            Some(Candidate {
                rule: rule.clone(),
                resolved_path: target,
                allocated_size: result.root.allocated_size,
                file_count: result.stats.total_files,
            })
        })
        .collect();
    candidates.sort_by(|a, b| b.allocated_size.cmp(&a.allocated_size));
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn embedded_rules_parse_and_are_home_relative() {
        let rules = load_rules();
        assert!(rules.len() >= 10);
        for r in &rules {
            assert!(
                r.path.starts_with("~/"),
                "rule {} must target a home-relative path (safety guard)",
                r.id
            );
            assert!(matches!(r.safety.as_str(), "safe" | "warn"), "{}", r.id);
        }
        // id 중복 금지.
        let mut ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), rules.len());
    }

    #[test]
    fn detect_measures_matching_dirs() {
        let fake_home =
            std::env::temp_dir().join(format!("space-rules-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&fake_home);
        let target = fake_home.join("Library/Caches/Homebrew");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("pkg.tar.gz"), vec![0u8; 50_000]).unwrap();

        let candidates = detect(&fake_home);
        let brew = candidates
            .iter()
            .find(|c| c.rule.id == "homebrew-cache")
            .expect("homebrew cache candidate");
        assert_eq!(brew.file_count, 1);
        assert!(brew.allocated_size >= 50_000);
        // user-caches 룰도 같은 트리를 잡는다 (중첩은 UI에서 카테고리로 구분).
        assert!(candidates.iter().any(|c| c.rule.id == "user-caches"));

        fs::remove_dir_all(&fake_home).unwrap();
    }
}
