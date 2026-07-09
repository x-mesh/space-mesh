//! 공식 정리 커맨드 어댑터.
//!
//! 파일을 직접 지우는 대신 각 도구의 공식 cleanup 경로를 제안한다.
//! dry-run이 가능한 도구는 실행해 예상 회수량을 파싱한다. 실행은 하지 않는다 —
//! 커맨드 제안 + 근거(dry-run 출력)까지만.

use rayon::prelude::*;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct ToolAdvice {
    pub tool: String,
    /// 사용자가 직접 실행할 공식 커맨드.
    pub command: String,
    pub description: String,
    /// dry-run에서 파싱한 예상 회수량 (bytes).
    pub estimated_reclaim: Option<u64>,
    /// 도구가 설치되어 있고 조회에 성공했는지.
    pub available: bool,
    /// dry-run 요약 또는 실패 사유.
    pub detail: String,
}

fn which(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run(bin: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(bin).args(args).output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

/// "1.2GB", "512.3MB", "4,096KB" 류 표기를 bytes로.
pub fn parse_size(text: &str) -> Option<u64> {
    let cleaned = text.trim().replace(',', "");
    let idx = cleaned.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = cleaned.split_at(idx);
    let value: f64 = num.trim().parse().ok()?;
    let mult: f64 = match unit.trim().to_uppercase().as_str() {
        "B" => 1.0,
        "KB" | "KIB" => 1024.0,
        "MB" | "MIB" => 1024.0 * 1024.0,
        "GB" | "GIB" => 1024.0 * 1024.0 * 1024.0,
        "TB" | "TIB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * mult) as u64)
}

/// `brew cleanup -n` 출력에서 예상 회수량 추출.
/// 마지막에 "This operation would free approximately 11.2GB of disk space." 형태가 온다.
pub fn parse_brew_cleanup(output: &str) -> Option<u64> {
    for line in output.lines().rev() {
        if let Some(rest) = line.split("approximately").nth(1) {
            let token = rest.split_whitespace().next()?;
            return parse_size(token);
        }
    }
    None
}

/// `docker system df` 표 출력에서 RECLAIMABLE 열 합산.
/// 각 행 끝: "1.5GB (60%)" 또는 "0B".
pub fn parse_docker_df(output: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut seen = false;
    for line in output.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 2 {
            continue;
        }
        // RECLAIMABLE은 뒤에서 첫 번째(또는 퍼센트 괄호 앞) 토큰.
        let candidate = if cols[cols.len() - 1].starts_with('(') {
            cols[cols.len() - 2]
        } else {
            cols[cols.len() - 1]
        };
        if let Some(bytes) = parse_size(candidate) {
            total += bytes;
            seen = true;
        }
    }
    if seen {
        Some(total)
    } else {
        None
    }
}

fn brew_advice() -> Option<ToolAdvice> {
    if !which("brew") {
        return None;
    }
    let dry = run("brew", &["cleanup", "-n"]);
    let (estimated, detail, available) = match &dry {
        Some(out) => {
            let est = parse_brew_cleanup(out);
            let lines = out.lines().filter(|l| l.contains("Would remove")).count();
            (est, format!("dry-run: {}개 항목 제거 예정", lines), true)
        }
        None => (None, "brew cleanup -n 실행 실패".to_string(), false),
    };
    Some(ToolAdvice {
        tool: "Homebrew".into(),
        command: "brew cleanup".into(),
        description: "오래된 formula 버전과 다운로드 캐시를 정리합니다.".into(),
        estimated_reclaim: estimated,
        available,
        detail,
    })
}

fn docker_advice() -> Option<ToolAdvice> {
    if !which("docker") {
        return None;
    }
    let df = run("docker", &["system", "df"]);
    let (estimated, detail, available) = match &df {
        Some(out) => (
            parse_docker_df(out),
            "reclaimable = 미사용 이미지/컨테이너/볼륨/빌드캐시 합계".to_string(),
            true,
        ),
        None => (None, "Docker 데몬이 실행 중이 아닙니다".to_string(), false),
    };
    Some(ToolAdvice {
        tool: "Docker".into(),
        command: "docker system prune  # 볼륨까지: --volumes (주의)".into(),
        description: "미사용 컨테이너/이미지/네트워크/빌드캐시를 정리합니다. 볼륨은 데이터 손실 위험이 있어 별도 확인 필요.".into(),
        estimated_reclaim: estimated,
        available,
        detail,
    })
}

fn simctl_advice() -> Option<ToolAdvice> {
    if !which("xcrun") {
        return None;
    }
    let list = run("xcrun", &["simctl", "list", "devices", "-j"]);
    let (detail, available) = match &list {
        Some(out) => {
            let unavailable = out.matches("unavailable").count();
            (
                format!("unavailable 표시 항목 {}개 감지", unavailable),
                true,
            )
        }
        None => ("simctl 조회 실패 (Xcode 미설치?)".to_string(), false),
    };
    Some(ToolAdvice {
        tool: "iOS Simulator".into(),
        command: "xcrun simctl delete unavailable".into(),
        description: "현재 Xcode에서 사용할 수 없는 구버전 시뮬레이터 기기를 삭제합니다.".into(),
        estimated_reclaim: None,
        available,
        detail,
    })
}

fn pnpm_advice() -> Option<ToolAdvice> {
    if !which("pnpm") {
        return None;
    }
    Some(ToolAdvice {
        tool: "pnpm".into(),
        command: "pnpm store prune".into(),
        description: "어떤 프로젝트도 참조하지 않는 고아 패키지를 store에서 제거합니다.".into(),
        estimated_reclaim: None,
        available: true,
        detail: "안전 — 참조 중인 패키지는 건드리지 않음".into(),
    })
}

fn pip_advice() -> Option<ToolAdvice> {
    if !which("pip3") {
        return None;
    }
    Some(ToolAdvice {
        tool: "pip".into(),
        command: "pip3 cache purge".into(),
        description: "pip 다운로드 캐시를 비웁니다. 다음 install 때 재다운로드됩니다.".into(),
        estimated_reclaim: None,
        available: true,
        detail: "안전 — 캐시 전용".into(),
    })
}

/// 설치된 도구들의 공식 정리 커맨드 제안 (병렬 조회, 실행은 안 함).
pub fn advise() -> Vec<ToolAdvice> {
    let probes: Vec<fn() -> Option<ToolAdvice>> = vec![
        brew_advice,
        docker_advice,
        simctl_advice,
        pnpm_advice,
        pip_advice,
    ];
    let mut advices: Vec<ToolAdvice> = probes.par_iter().filter_map(|f| f()).collect();
    advices.sort_by(|a, b| {
        b.estimated_reclaim
            .unwrap_or(0)
            .cmp(&a.estimated_reclaim.unwrap_or(0))
    });
    advices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("1.5GB"), Some((1.5 * 1073741824.0) as u64));
        assert_eq!(parse_size("512MB"), Some(512 * 1048576));
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("(60%)"), None);
    }

    #[test]
    fn parses_brew_cleanup_total() {
        let out = "Would remove: /opt/homebrew/Cellar/node/20.0.0 (3,032 files, 55MB)\n\
                   Would remove: /Users/x/Library/Caches/Homebrew/go--1.22.tar.gz (120MB)\n\
                   This operation would free approximately 11.2GB of disk space.";
        assert_eq!(parse_brew_cleanup(out), Some((11.2 * 1073741824.0) as u64));
    }

    #[test]
    fn parses_docker_df_reclaimable() {
        let out = "TYPE            TOTAL     ACTIVE    SIZE      RECLAIMABLE\n\
                   Images          12        3         14.2GB    9.5GB (66%)\n\
                   Containers      5         1         120MB     100MB (83%)\n\
                   Local Volumes   8         2         2GB       1.5GB (75%)\n\
                   Build Cache     101       0         3.1GB     3.1GB";
        let total = parse_docker_df(out).unwrap();
        let expected = parse_size("9.5GB").unwrap()
            + parse_size("100MB").unwrap()
            + parse_size("1.5GB").unwrap()
            + parse_size("3.1GB").unwrap();
        assert_eq!(total, expected);
    }
}
