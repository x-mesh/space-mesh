//! 잘 알려진 빌드 산출물/캐시 디렉토리의 카테고리 탐지.
//!
//! 고정 경로 룰(rules.json)과 달리, 스캔된 트리 전체에서 이름 패턴으로 흩어진
//! 인스턴스를 찾는다. 모호한 이름(target, build, vendor)은 프로젝트 마커
//! (Cargo.toml, build.gradle 등)가 확인될 때만 인정해 오탐을 막는다.
//! 매치된 디렉토리 내부로는 내려가지 않아 중첩 이중 계산이 없다.

use space_scanner::DirNode;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct CategoryDef {
    pub id: &'static str,
    pub title: &'static str,
    pub dir_names: &'static [&'static str],
    /// 부모 디렉토리에 존재해야 하는 마커 (하나라도 있으면 verified).
    pub parent_markers: &'static [&'static str],
    /// 매치된 디렉토리 내부의 마커.
    pub inner_markers: &'static [&'static str],
    /// true면 마커 미확인 시 히트에서 제외 (모호한 이름).
    pub require_marker: bool,
    pub safety: &'static str,
    pub description: &'static str,
    /// 삭제 후 복원하는 명령 (빈 문자열 = 자동 재생성).
    pub recreate_command: &'static str,
    /// 재생성 비용: low(자동/빠름) | medium(다운로드) | high(재빌드 CPU).
    pub recreate_cost: &'static str,
}

pub const CATEGORIES: &[CategoryDef] = &[
    CategoryDef {
        id: "node-modules",
        title: "node_modules",
        dir_names: &["node_modules"],
        parent_markers: &["package.json"],
        inner_markers: &[],
        require_marker: false,
        safety: "safe",
        description: "JavaScript 프로젝트가 내려받은 외부 라이브러리 폴더입니다. 프로젝트마다 하나씩 생기며 수십만 개의 작은 파일로 이뤄집니다. 지워도 npm/pnpm/yarn install 한 번이면 그대로 재생성됩니다.",
        recreate_command: "npm install  # 또는 pnpm/yarn install",
        recreate_cost: "medium",
    },
    CategoryDef {
        id: "cargo-target",
        title: "Cargo target",
        dir_names: &["target"],
        parent_markers: &["Cargo.toml"],
        inner_markers: &[],
        require_marker: true,
        safety: "safe",
        description: "Rust 컴파일러가 만든 중간 산출물·실행 파일입니다. 프로젝트 소스와 무관하며, 지워도 cargo build 한 번이면 재생성됩니다 (빌드 시간만 다시 듭니다).",
        recreate_command: "cargo build",
        recreate_cost: "high",
    },
    CategoryDef {
        id: "python-venv",
        title: "Python venv",
        dir_names: &[".venv", "venv"],
        parent_markers: &[],
        inner_markers: &["pyvenv.cfg"],
        require_marker: true,
        safety: "safe",
        description: "Python 프로젝트 전용으로 설치된 패키지 묶음(가상환경)입니다. 소스 코드는 들어있지 않으며, pip/uv/poetry install로 재생성됩니다.",
        recreate_command: "uv sync  # 또는 pip install -r requirements.txt",
        recreate_cost: "medium",
    },
    CategoryDef {
        id: "pycache",
        title: "__pycache__",
        dir_names: &["__pycache__"],
        parent_markers: &[],
        inner_markers: &[],
        require_marker: false,
        safety: "safe",
        description: "Python이 실행 속도를 위해 만든 바이트코드 캐시입니다. 지워도 다음 실행 때 자동으로 다시 생깁니다.",
        recreate_command: "",
        recreate_cost: "low",
    },
    CategoryDef {
        id: "python-tool-cache",
        title: "Python 도구 캐시",
        dir_names: &[".pytest_cache", ".mypy_cache", ".ruff_cache", ".tox"],
        parent_markers: &[],
        inner_markers: &[],
        require_marker: false,
        safety: "safe",
        description: "테스트·린트 도구(pytest/mypy/ruff/tox)가 만든 캐시입니다. 지워도 다음 실행 때 자동으로 다시 생깁니다.",
        recreate_command: "",
        recreate_cost: "low",
    },
    CategoryDef {
        id: "gradle-build",
        title: "Gradle build",
        dir_names: &["build"],
        parent_markers: &[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
        inner_markers: &[],
        require_marker: true,
        safety: "safe",
        description: "Gradle(Android/Java)이 만든 빌드 산출물입니다. 소스와 무관하며 다음 빌드에서 재생성됩니다.",
        recreate_command: "./gradlew build",
        recreate_cost: "high",
    },
    CategoryDef {
        id: "gradle-project-cache",
        title: "Gradle 프로젝트 캐시 (.gradle)",
        dir_names: &[".gradle"],
        parent_markers: &[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
        inner_markers: &[],
        require_marker: true,
        safety: "safe",
        description: "프로젝트 폴더 안의 Gradle 작업 캐시입니다. 지워도 다음 빌드 때 자동 재생성됩니다.",
        recreate_command: "",
        recreate_cost: "low",
    },
    CategoryDef {
        id: "cocoapods",
        title: "CocoaPods Pods",
        dir_names: &["Pods"],
        parent_markers: &["Podfile"],
        inner_markers: &[],
        require_marker: true,
        safety: "safe",
        description: "iOS/macOS 프로젝트가 내려받은 CocoaPods 라이브러리입니다. pod install 한 번이면 재생성됩니다.",
        recreate_command: "pod install",
        recreate_cost: "medium",
    },
    CategoryDef {
        id: "next-build",
        title: ".next 빌드",
        dir_names: &[".next"],
        parent_markers: &["package.json"],
        inner_markers: &[],
        require_marker: true,
        safety: "safe",
        description: "Next.js가 만든 빌드 결과물과 캐시입니다. next build/dev 실행 시 재생성됩니다.",
        recreate_command: "next build",
        recreate_cost: "medium",
    },
    CategoryDef {
        id: "terraform",
        title: ".terraform",
        dir_names: &[".terraform"],
        parent_markers: &[".terraform.lock.hcl"],
        inner_markers: &[],
        require_marker: true,
        safety: "warn",
        description: "Terraform이 내려받은 provider 플러그인입니다. 지우면 다음 작업 전에 terraform init으로 다시 받아야 합니다.",
        recreate_command: "terraform init",
        recreate_cost: "medium",
    },
    CategoryDef {
        id: "go-vendor",
        title: "Go vendor",
        dir_names: &["vendor"],
        parent_markers: &["go.mod"],
        inner_markers: &[],
        require_marker: true,
        safety: "safe",
        description: "Go 프로젝트가 복사해둔 의존성 소스입니다. go mod vendor 한 번이면 재생성됩니다.",
        recreate_command: "go mod vendor",
        recreate_cost: "low",
    },
    CategoryDef {
        id: "turbo-cache",
        title: ".turbo 캐시",
        dir_names: &[".turbo"],
        parent_markers: &["package.json"],
        inner_markers: &[],
        require_marker: true,
        safety: "safe",
        description: "Turborepo 빌드 캐시입니다. 지워도 다음 빌드 때 자동으로 다시 생깁니다.",
        recreate_command: "",
        recreate_cost: "low",
    },
];

#[derive(Debug)]
pub struct CategoryHit {
    pub def: &'static CategoryDef,
    pub path: PathBuf,
    /// 이 산출물이 속한 프로젝트 디렉토리 (부모).
    pub project_path: PathBuf,
    pub allocated_size: u64,
    pub file_count: u64,
    /// 마커로 정체가 확인되었는지 (require_marker=false 카테고리에서만 false 가능).
    pub verified: bool,
}

fn match_def(name: &str) -> Option<&'static CategoryDef> {
    CATEGORIES
        .iter()
        .find(|def| def.dir_names.contains(&name))
}

/// 스캔된 트리에서 카테고리 히트를 찾는다. 트리는 메모리에 있으므로 빠르고,
/// 마커 확인만 파일시스템을 조회한다.
pub fn find_categories(root: &DirNode, root_path: &Path) -> Vec<CategoryHit> {
    let mut hits = Vec::new();
    walk(root, root_path, &mut hits);
    hits.sort_by(|a, b| b.allocated_size.cmp(&a.allocated_size));
    hits
}

fn walk(node: &DirNode, path: &Path, hits: &mut Vec<CategoryHit>) {
    for child in &node.children {
        let child_path = path.join(&child.name);
        if let Some(def) = match_def(&child.name) {
            let verified = def
                .parent_markers
                .iter()
                .any(|m| path.join(m).exists())
                || def
                    .inner_markers
                    .iter()
                    .any(|m| child_path.join(m).exists());
            if verified || !def.require_marker {
                if child.allocated_size > 0 {
                    hits.push(CategoryHit {
                        def,
                        path: child_path,
                        project_path: path.to_path_buf(),
                        allocated_size: child.allocated_size,
                        file_count: child.file_count,
                        verified: verified
                            || (def.parent_markers.is_empty() && def.inner_markers.is_empty()),
                    });
                }
                // 매치된 디렉토리 내부로는 내려가지 않는다 (중첩 이중 계산 방지).
                continue;
            }
        }
        walk(child, &child_path, hits);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space_scanner::{scan, ScanOptions};
    use std::fs;

    #[test]
    fn detects_verified_and_skips_unmarked_ambiguous() {
        let tmp = std::env::temp_dir().join(format!("space-cat-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);

        // rust 프로젝트: Cargo.toml + target → 히트
        fs::create_dir_all(tmp.join("proj-rs/target/debug")).unwrap();
        fs::write(tmp.join("proj-rs/Cargo.toml"), "[package]").unwrap();
        fs::write(tmp.join("proj-rs/target/debug/bin"), vec![0u8; 9000]).unwrap();

        // 마커 없는 target → 모호하므로 제외
        fs::create_dir_all(tmp.join("random/target")).unwrap();
        fs::write(tmp.join("random/target/data"), vec![0u8; 5000]).unwrap();

        // node_modules (마커 있음 → verified)
        fs::create_dir_all(tmp.join("proj-js/node_modules/lodash")).unwrap();
        fs::write(tmp.join("proj-js/package.json"), "{}").unwrap();
        fs::write(
            tmp.join("proj-js/node_modules/lodash/index.js"),
            vec![0u8; 7000],
        )
        .unwrap();

        // 중첩 node_modules — 바깥 것만 잡혀야 함
        fs::create_dir_all(tmp.join("proj-js/node_modules/pkg/node_modules")).unwrap();

        let result = scan(&tmp, ScanOptions::default()).unwrap();
        let hits = find_categories(&result.root, &tmp);

        let ids: Vec<&str> = hits.iter().map(|h| h.def.id).collect();
        assert!(ids.contains(&"cargo-target"), "{:?}", ids);
        assert!(ids.contains(&"node-modules"), "{:?}", ids);
        assert_eq!(
            ids.iter().filter(|&&i| i == "node-modules").count(),
            1,
            "중첩 node_modules는 한 번만: {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert_eq!(
            ids.iter().filter(|&&i| i == "cargo-target").count(),
            1,
            "마커 없는 target은 제외"
        );
        let nm = hits.iter().find(|h| h.def.id == "node-modules").unwrap();
        assert!(nm.verified);
        assert!(nm.project_path.ends_with("proj-js"));

        fs::remove_dir_all(&tmp).unwrap();
    }
}

// ───────────────────────── 유휴 프로젝트 탐지 (git 마지막 커밋) ─────────────────────────

/// 프로젝트 경로의 git 마지막 커밋이 며칠 전인지. .git이 없거나 git 실패 시 None.
pub fn git_last_commit_days(project: &Path) -> Option<u64> {
    if !project.join(".git").exists() {
        return None;
    }
    let output = std::process::Command::new("git")
        .args(["-C"])
        .arg(project)
        .args(["log", "-1", "--format=%ct"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let epoch: u64 = String::from_utf8_lossy(&output.stdout).trim().parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(epoch) / 86_400)
}

/// 히트 목록의 프로젝트별 유휴 일수를 병렬로 채운다 (프로젝트당 git 1회).
pub fn annotate_idle(hits: &[CategoryHit]) -> std::collections::HashMap<PathBuf, u64> {
    use rayon::prelude::*;
    let mut projects: Vec<&PathBuf> = hits.iter().map(|h| &h.project_path).collect();
    projects.sort_unstable();
    projects.dedup();
    projects
        .par_iter()
        .filter_map(|p| git_last_commit_days(p).map(|d| ((*p).clone(), d)))
        .collect()
}
