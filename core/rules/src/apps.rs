//! /Applications 앱 회계 — 크기와 "마지막으로 쓴 날"을 함께 본다.
//!
//! 기본 스캔 루트가 홈이라 /Applications(실측 46 GiB)는 트리맵에 잡히지 않는다.
//! 여기서는 스캔 트리와 무관하게 앱을 직접 열거해 "크고 안 쓰는 앱"을 짚어준다.
//!
//! 안전 불변식 두 가지:
//!   1. 삭제 대상은 **번들뿐**이다. `data_size`가 가리키는 ~/Library 경로는 표시만 한다 —
//!      Application Support에는 라이선스 키·노트 DB처럼 재생성 불가능한 사용자 데이터가 산다.
//!   2. 판정에 실패하면 **보호 쪽으로** 기운다. 서명을 못 읽으면 Apple 앱으로 간주해 삭제를 막고,
//!      사용 시각을 모르면 "미사용"이 아니라 `None`(기록 없음)으로 남긴다.

use plist::Value;
use rayon::prelude::*;
use space_scanner::{scan, ScanOptions};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// mdls 배치 출력에서 "값 없음"을 나타내는 마커. 날짜에 공백이 섞이므로
/// 공백으로는 경계를 못 나눈다 — -raw 출력은 NUL로 구분되고, 이 마커가 빈 값 자리를 채운다.
const NULL_MARKER: &str = "\u{1}NONE";

const APPLICATIONS_DIR: &str = "/Applications";

#[derive(Debug, Clone)]
pub struct AppEntry {
    pub name: String,
    pub path: PathBuf,
    pub bundle_id: String,
    /// 번들 자체의 allocated 크기 — 삭제하면 실제로 돌아오는 용량.
    pub allocated_size: u64,
    /// 마지막 사용으로부터 지난 일수. `None`은 "안 씀"이 아니라 **"기록 없음"**이다.
    pub last_used_days: Option<u64>,
    /// "brew" | "mas" | "unknown"
    pub source: String,
    /// 지우고 나서 되돌리는 방법. 모르면 빈 문자열 → UI에서 경고.
    pub recreate_command: String,
    /// Apple 서명 앱(Safari 등)은 삭제를 막는다. 서명을 못 읽어도 true(보호).
    pub is_apple: bool,
}

/// /Applications의 앱을 열거해 회계 정보를 채운다.
/// 스캔 핸들에 의존하지 않으므로 스캔 전에도 호출할 수 있다.
pub fn list_apps() -> Vec<AppEntry> {
    let bundles = collect_bundles(Path::new(APPLICATIONS_DIR));
    if bundles.is_empty() {
        return Vec::new();
    }

    // mdls는 앱마다 부르면 100개에 2.4초가 든다. 한 번에 몰아 부르면 0.09초다.
    let last_used = last_used_days_batch(&bundles);
    let brew_casks = brew_cask_tokens();

    bundles
        .into_par_iter()
        .zip(last_used.into_par_iter())
        .map(|(path, mdls_days)| {
            let name = bundle_name(&path);
            let info = read_info_plist(&path);
            let bundle_id = info.bundle_id;

            // mdls에 기록이 없는 앱이 29%였다 (Xcode 포함). 실행 파일의 atime이 그 구멍을 메운다 —
            // 이 폴백이 없으면 매일 쓰는 Xcode를 "안 씀"으로 지목하게 된다.
            let last_used_days =
                mdls_days.or_else(|| executable_atime_days(&path, &info.executable));

            let from_app_store = has_mas_receipt(&path);
            let (source, recreate_command) =
                resolve_source(from_app_store, &name, &bundle_id, &brew_casks);

            AppEntry {
                allocated_size: bundle_allocated_size(&path),
                is_apple: is_apple_system_app(&bundle_id, from_app_store),
                name,
                path,
                bundle_id,
                last_used_days,
                source,
                recreate_command,
            }
        })
        .collect()
}

// ───────────────────────── 열거 ─────────────────────────

/// `/Applications/*.app`. 디렉터리가 없거나 비어도 에러가 아니라 빈 목록이다.
fn collect_bundles(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut bundles: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "app"))
        .collect();
    bundles.sort();
    bundles
}

fn bundle_name(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn bundle_allocated_size(path: &Path) -> u64 {
    scan(
        path,
        ScanOptions {
            record_file_threshold: u64::MAX, // 개별 파일 목록은 필요 없다
            one_filesystem: false,
        },
    )
    .map(|r| r.root.allocated_size)
    .unwrap_or(0)
}

// ───────────────────────── Info.plist ─────────────────────────

#[derive(Default)]
struct BundleInfo {
    bundle_id: String,
    executable: String,
}

/// Info.plist는 대개 바이너리 plist다. plutil을 앱마다 띄우면 100개에 2초가 넘으므로
/// 프로세스 없이 직접 읽는다.
fn read_info_plist(bundle: &Path) -> BundleInfo {
    let plist_path = bundle.join("Contents/Info.plist");
    let Ok(Value::Dictionary(dict)) = Value::from_file(&plist_path) else {
        return BundleInfo::default();
    };
    let get = |key: &str| -> String {
        dict.get(key)
            .and_then(|v| v.as_string())
            .unwrap_or_default()
            .to_string()
    };
    BundleInfo {
        bundle_id: get("CFBundleIdentifier"),
        executable: get("CFBundleExecutable"),
    }
}

// ───────────────────────── 마지막 사용일 ─────────────────────────

/// 전체 앱의 kMDItemLastUsedDate를 한 번의 mdls 호출로 가져온다.
/// 반환 벡터는 입력 `bundles`와 순서·길이가 1:1로 대응한다.
fn last_used_days_batch(bundles: &[PathBuf]) -> Vec<Option<u64>> {
    let none = || vec![None; bundles.len()];

    let output = Command::new("/usr/bin/mdls")
        .arg("-name")
        .arg("kMDItemLastUsedDate")
        .arg("-raw")
        .arg("-nullMarker")
        .arg(NULL_MARKER)
        .args(bundles) // 경로를 인자로 그대로 넘긴다 — 셸을 거치지 않으므로 공백·유니코드가 안전하다
        .output();

    let Ok(output) = output else { return none() };
    if !output.status.success() {
        return none();
    }

    // -raw 출력은 값들을 NUL로 구분해 이어 붙인다. 날짜 자체에 공백이 있어서
    // 공백으로 자르면 경계가 깨진다.
    let raw = String::from_utf8_lossy(&output.stdout);
    let values: Vec<&str> = raw.split('\0').collect();
    if values.len() < bundles.len() {
        return none();
    }

    values
        .iter()
        .take(bundles.len())
        .map(|v| parse_mdls_date(v.trim()))
        .collect()
}

/// mdls 날짜 형식: `2026-03-08 14:19:30 +0000`. 마커거나 못 읽으면 None.
fn parse_mdls_date(value: &str) -> Option<u64> {
    if value.is_empty() || value == NULL_MARKER || value == "(null)" {
        return None;
    }
    let date = value.split_whitespace().next()?; // "2026-03-08"
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    days_since(days_from_civil(year, month, day))
}

/// 번들 실행 파일의 atime. mdls가 비어 있는 앱(실측 29%)을 여기서 건진다.
fn executable_atime_days(bundle: &Path, executable: &str) -> Option<u64> {
    let exe = if executable.is_empty() {
        // Info.plist를 못 읽었으면 Contents/MacOS의 첫 파일을 실행 파일로 본다.
        std::fs::read_dir(bundle.join("Contents/MacOS"))
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.is_file())?
    } else {
        bundle.join("Contents/MacOS").join(executable)
    };

    let accessed = std::fs::metadata(&exe).ok()?.accessed().ok()?;
    let secs = accessed.duration_since(UNIX_EPOCH).ok()?.as_secs();
    days_since((secs / 86_400) as i64)
}

/// 유닉스 epoch 기준 일수 → 오늘로부터 지난 일수. 미래 시각은 0일로 본다.
fn days_since(epoch_days: i64) -> Option<u64> {
    let now_days = (SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() / 86_400) as i64;
    Some((now_days - epoch_days).max(0) as u64)
}

/// 그레고리력 → 유닉스 epoch 기준 일수 (Howard Hinnant days_from_civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ───────────────────────── 복원 명령 ─────────────────────────

/// `brew list --cask`로 설치된 cask 토큰. brew가 없거나 실패해도 에러가 아니라 빈 집합이다
/// (모든 앱이 "복원 명령 불명"으로 떨어질 뿐, 기능이 죽지는 않는다).
fn brew_cask_tokens() -> HashSet<String> {
    let output = Command::new("brew").arg("list").arg("--cask").output();
    let Ok(output) = output else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// App Store로 설치된 앱은 영수증을 품고 있다 — 확실한 판별이다.
fn has_mas_receipt(bundle: &Path) -> bool {
    bundle.join("Contents/_MASReceipt/receipt").exists()
}

/// 앱 이름 → cask 토큰 후보들. 관례는 소문자 + 공백은 하이픈이지만
/// 구두점 처리가 제각각이라 몇 가지 변형을 함께 시도한다.
fn cask_token_candidates(name: &str, bundle_id: &str) -> Vec<String> {
    let lower = name.to_lowercase();
    let hyphenated: String = lower
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let squashed: String = lower.chars().filter(|c| c.is_alphanumeric()).collect();

    let mut out = vec![
        lower.replace(' ', "-"),
        // 연속 하이픈을 하나로 접는다: "app (beta)" → "app-beta"
        hyphenated
            .split('-')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-"),
        squashed,
    ];
    // 번들 id의 마지막 조각도 흔한 토큰이다 (com.foo.Bar → "bar").
    if let Some(last) = bundle_id.rsplit('.').next() {
        if !last.is_empty() {
            out.push(last.to_lowercase());
        }
    }
    out.dedup();
    out
}

fn resolve_source(
    from_app_store: bool,
    name: &str,
    bundle_id: &str,
    brew_casks: &HashSet<String>,
) -> (String, String) {
    if from_app_store {
        return ("mas".into(), "App Store에서 재설치".into());
    }
    for token in cask_token_candidates(name, bundle_id) {
        if brew_casks.contains(&token) {
            return ("brew".into(), format!("brew reinstall --cask {token}"));
        }
    }
    // 직접 내려받은 앱 — 되돌리는 법을 모른다. UI에서 경고 배지를 달아야 한다.
    ("unknown".into(), String::new())
}

// ───────────────────────── Apple 시스템 앱 보호 ─────────────────────────

/// 지우면 안 되는 Apple 시스템 앱인가 (Safari 등).
///
/// `com.apple.` 접두어만 보면 **Xcode도 걸린다** — 하지만 Xcode는 App Store에서 받은
/// 4 GB짜리 앱이고 재설치가 되므로 최대 삭제 후보다. 영수증이 있으면 시스템 앱이 아니다.
///
/// codesign을 쓰지 않는 이유: 앱마다 프로세스를 띄워 101개에 2.4초가 들고,
/// `TeamIdentifier=not set`은 서명 없는 서드파티 앱까지 잡아 과보호한다.
/// bundle id는 이미 읽어둔 값이라 공짜다.
fn is_apple_system_app(bundle_id: &str, from_app_store: bool) -> bool {
    if from_app_store {
        return false; // App Store 앱은 재설치 가능 — Xcode가 여기 해당한다
    }
    // Info.plist를 못 읽어 id가 비었으면 정체를 모른다 → 보호 쪽으로 기운다.
    bundle_id.is_empty() || bundle_id.starts_with("com.apple.")
}

// ───────────────────────── 앱 데이터 (표시 전용) ─────────────────────────

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// bundle id로 확실히 그 앱의 것이라 말할 수 있는 ~/Library 데이터의 합.
/// **삭제 대상이 아니다 — 보여주기만 한다.**
///
/// 목록을 만들 때 전부 계산하면 101개에 4초가 든다 — 번들 크기를 재는 본 작업(2.5초)보다 비싸다.
/// 그래서 사용자가 앱을 고른 순간에만 부른다.
///
/// `Group Containers`는 일부러 제외한다. 팀 id로 키가 잡히고(`UBF8T346G9.Office`)
/// 여러 앱이 함께 쓴다 — Office 353 MB를 Outlook·Word·Excel에 각각 더하면 세 배로 부풀려진다.
/// 귀속할 수 없는 데이터는 세지 않는 편이 정직하다. 이 값은 "그 앱 전용 데이터"이지
/// "앱이 쓰는 전체 데이터"가 아니다.
///
/// bundle_id가 비면 0을 돌려준다 — 빈 문자열로 경로를 만들면 ~/Library 전체를 훑게 된다.
pub fn app_data_size(bundle_id: &str) -> u64 {
    data_size_for(bundle_id, home_dir().as_deref())
}

fn data_size_for(bundle_id: &str, home: Option<&Path>) -> u64 {
    if bundle_id.is_empty() {
        return 0;
    }
    let Some(home) = home else { return 0 };

    let candidates = [
        home.join("Library/Containers").join(bundle_id),
        home.join("Library/Caches").join(bundle_id),
        home.join("Library/Application Support").join(bundle_id),
        home.join("Library/Preferences")
            .join(format!("{bundle_id}.plist")),
        home.join("Library/Saved Application State")
            .join(format!("{bundle_id}.savedState")),
        home.join("Library/Logs").join(bundle_id),
    ];

    candidates
        .iter()
        .filter(|p| p.exists())
        .map(|p| {
            if p.is_dir() {
                bundle_allocated_size(p)
            } else {
                std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
            }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_marker_and_empty_parse_to_none() {
        assert_eq!(parse_mdls_date(NULL_MARKER), None);
        assert_eq!(parse_mdls_date(""), None);
        assert_eq!(parse_mdls_date("(null)"), None);
    }

    #[test]
    fn mdls_date_parses_to_elapsed_days() {
        // 과거 날짜는 양수 일수로 나와야 한다.
        let days = parse_mdls_date("2020-01-01 00:00:00 +0000").expect("과거 날짜는 파싱된다");
        assert!(days > 1_000, "2020년이면 1000일은 넘었다: {days}");
    }

    #[test]
    fn days_from_civil_matches_known_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 1, 1), 10_957);
    }

    #[test]
    fn cask_token_candidates_cover_common_conventions() {
        let c = cask_token_candidates("Google Chrome", "com.google.Chrome");
        assert!(c.contains(&"google-chrome".to_string()));
        let c = cask_token_candidates("Another Redis Desktop Manager", "");
        assert!(c.contains(&"another-redis-desktop-manager".to_string()));
    }

    /// Xcode는 com.apple.* 이지만 App Store에서 받은 4 GB짜리 앱이라 지울 수 있어야 한다.
    /// 접두어만 보고 막으면 최대 삭제 후보를 잠가버린다.
    #[test]
    fn app_store_apple_apps_stay_deletable() {
        assert!(
            !is_apple_system_app("com.apple.dt.Xcode", true),
            "Xcode는 삭제 가능해야 한다"
        );
        assert!(
            is_apple_system_app("com.apple.Safari", false),
            "Safari는 보호되어야 한다"
        );
        assert!(!is_apple_system_app("com.google.Chrome", false));
    }

    /// 정체를 모르면 삭제를 허용하지 말고 보호한다.
    #[test]
    fn unknown_bundle_id_is_protected() {
        assert!(is_apple_system_app("", false));
    }

    #[test]
    fn app_store_receipt_wins_over_brew() {
        let casks: HashSet<String> = ["xcode".to_string()].into_iter().collect();
        let (source, cmd) = resolve_source(true, "Xcode", "com.apple.dt.Xcode", &casks);
        assert_eq!(source, "mas");
        assert!(cmd.contains("App Store"));
    }

    #[test]
    fn unmatched_app_reports_unknown_with_no_command() {
        let (source, cmd) =
            resolve_source(false, "Some Direct Download", "com.x.y", &HashSet::new());
        assert_eq!(source, "unknown");
        assert!(cmd.is_empty(), "복원법을 모르면 명령을 지어내지 않는다");
    }

    #[test]
    fn empty_bundle_id_never_scans_library() {
        // 빈 id로 ~/Library 전체를 훑는 사고를 막는다.
        assert_eq!(data_size_for("", home_dir().as_deref()), 0);
    }

    #[test]
    fn missing_applications_dir_yields_empty_list() {
        assert!(collect_bundles(Path::new("/nonexistent-apps-dir")).is_empty());
    }
}
