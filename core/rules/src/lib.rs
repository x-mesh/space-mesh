//! 안전등급 태깅된 룰셋 기반 불필요 파일 탐지.
//!
//! 룰은 rules.json(빌드에 내장)으로 관리한다 — 스키마는 외부 룰 파일 로드도 지원해
//! 앱 업데이트 없이 갱신 가능한 구조를 유지한다.
//! (계획상 YAML이었으나 serde_yaml 미유지보수로 JSON 채택.)

pub mod advisor;
pub mod apps;
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
    /// 이 항목만의 크기. 다른 룰이 이 경로 안을 따로 잡고 있으면 그만큼 빠져 있다 —
    /// 항목들이 서로 겹치지 않으므로 어디서 더해도 실제 회수량과 일치한다.
    pub allocated_size: u64,
    pub file_count: u64,
    /// 실제로 휴지통에 보낼 경로들.
    ///
    /// 보통은 `resolved_path` 하나다. 다른 룰이 이 경로 안을 따로 잡고 있으면
    /// (예: `~/Library/Caches`가 Homebrew·pip·Yarn 캐시를 품는다) 그 자식들을 뺀
    /// 나머지 항목들이 들어간다 — 디렉터리를 통째로 지우면 따로 고를 수 있어야 할
    /// 항목까지 함께 날아가기 때문이다.
    pub delete_paths: Vec<PathBuf>,
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
    let measured: Vec<Candidate> = rules
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
                delete_paths: vec![target.clone()],
                resolved_path: target,
                allocated_size: result.root.allocated_size,
                file_count: result.stats.total_files,
            })
        })
        .collect();

    let mut candidates = subtract_nested(measured);
    candidates.sort_by_key(|f| std::cmp::Reverse(f.allocated_size));
    candidates
}

/// 룰끼리 경로가 겹치면 바깥 항목에서 안쪽 항목을 뺀다.
///
/// `~/Library/Caches`는 Homebrew·pip·Yarn 캐시를 품는다. 둘 다 그대로 두면 같은 바이트를
/// 두 번 세게 되고, 장바구니가 실제보다 큰 회수량을 약속하게 된다 — 디스크 도구가 회수량을
/// 부풀려 말하는 건 그 자체로 고장이다. (`categories.rs`는 매치된 디렉터리 안으로 내려가지
/// 않는 방식으로 같은 문제를 막는다.)
///
/// 안쪽 항목을 버리지 않고 바깥 항목을 "나머지"로 바꾸는 이유: 안쪽 항목이 더 정밀하고
/// 대개 더 안전하다. Homebrew 캐시만 지우는 선택지를 없애면 안 된다.
fn subtract_nested(measured: Vec<Candidate>) -> Vec<Candidate> {
    // (경로, 크기, 파일 수) 스냅샷 — 차감할 때 원본을 그대로 참조한다.
    let sizes: Vec<(PathBuf, u64, u64)> = measured
        .iter()
        .map(|c| (c.resolved_path.clone(), c.allocated_size, c.file_count))
        .collect();

    measured
        .into_iter()
        .filter_map(|mut c| {
            let nested: Vec<&(PathBuf, u64, u64)> = sizes
                .iter()
                .filter(|(p, _, _)| p != &c.resolved_path && p.starts_with(&c.resolved_path))
                .collect();
            if nested.is_empty() {
                return Some(c);
            }

            // 같은 스캐너로 잰 같은 트리라 정확히 상쇄된다.
            let nested_size: u64 = nested.iter().map(|(_, s, _)| s).sum();
            let nested_files: u64 = nested.iter().map(|(_, _, f)| f).sum();
            c.allocated_size = c.allocated_size.saturating_sub(nested_size);
            c.file_count = c.file_count.saturating_sub(nested_files);

            // 남은 게 없으면 항목 자체를 없앤다 — "기타 0 B"를 보여줄 이유가 없다.
            if c.allocated_size == 0 {
                return None;
            }

            let nested_paths: Vec<&PathBuf> = nested.iter().map(|(p, _, _)| p).collect();
            c.delete_paths = residual_paths(&c.resolved_path, &nested_paths);
            Some(c)
        })
        .collect()
}

/// 나머지 경로들 — `root` 아래에서 `nested`에 해당하지 않는 항목만 고른다.
/// `nested`가 더 깊이 있으면 그 조상으로 내려가며 쪼갠다.
fn residual_paths(root: &Path, nested: &[&PathBuf]) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if nested.iter().any(|n| n.as_path() == path) {
            continue; // 이 항목은 별도 룰이 따로 잡는다
        }
        if nested.iter().any(|n| n.starts_with(&path)) {
            // 자식이 이 항목 더 안쪽에 있다 — 통째로는 못 지우니 한 단계 더 들어간다.
            out.extend(residual_paths(&path, nested));
        } else {
            out.push(path);
        }
    }
    out
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

        fs::remove_dir_all(&fake_home).unwrap();
    }

    /// ~/Library/Caches는 Homebrew·pip 캐시를 품는다. 둘을 그대로 더하면 같은 바이트를
    /// 두 번 세어, 장바구니가 실제보다 큰 회수량을 약속하게 된다.
    #[test]
    fn nested_rules_are_not_counted_twice() {
        let fake_home =
            std::env::temp_dir().join(format!("space-rules-nested-{}", std::process::id()));
        let _ = fs::remove_dir_all(&fake_home);
        let caches = fake_home.join("Library/Caches");

        // 별도 룰이 잡는 자식들
        fs::create_dir_all(caches.join("Homebrew")).unwrap();
        fs::write(caches.join("Homebrew/pkg.tar.gz"), vec![0u8; 40_000]).unwrap();
        fs::create_dir_all(caches.join("pip")).unwrap();
        fs::write(caches.join("pip/wheel"), vec![0u8; 20_000]).unwrap();
        // 어떤 룰도 잡지 않는 나머지
        fs::create_dir_all(caches.join("SomeOtherApp")).unwrap();
        fs::write(caches.join("SomeOtherApp/blob"), vec![0u8; 10_000]).unwrap();

        let candidates = detect(&fake_home);
        let get = |id: &str| candidates.iter().find(|c| c.rule.id == id);

        let brew = get("homebrew-cache").expect("homebrew");
        let pip = get("pip-cache").expect("pip");
        let rest = get("user-caches").expect("나머지 앱 캐시");

        // 나머지는 자식들을 뺀 크기여야 한다 — 자식 크기를 다시 품고 있으면 안 된다.
        assert!(
            rest.allocated_size < brew.allocated_size,
            "나머지({})가 Homebrew({})보다 크면 자식을 아직 품고 있다는 뜻",
            rest.allocated_size,
            brew.allocated_size
        );
        assert!(
            rest.allocated_size < pip.allocated_size + brew.allocated_size,
            "나머지가 자식들 합보다 크면 차감이 안 된 것"
        );

        // 전부 골랐을 때의 합이 실제 트리 크기를 넘지 않아야 한다.
        let sum: u64 = candidates.iter().map(|c| c.allocated_size).sum();
        let actual = scan(
            &caches,
            ScanOptions {
                record_file_threshold: u64::MAX,
                one_filesystem: false,
            },
        )
        .unwrap()
        .root
        .allocated_size;
        assert!(
            sum <= actual,
            "합계 {sum}가 실제 {actual}를 넘는다 — 회수량을 부풀려 말하고 있다"
        );

        // 나머지를 지울 때 자식 경로는 건드리지 않아야 한다.
        assert!(
            !rest.delete_paths.iter().any(|p| p.ends_with("Homebrew")),
            "나머지 삭제가 Homebrew 캐시까지 지우면 따로 고를 이유가 없어진다"
        );
        assert!(rest
            .delete_paths
            .iter()
            .any(|p| p.ends_with("SomeOtherApp")));

        fs::remove_dir_all(&fake_home).unwrap();
    }
}
