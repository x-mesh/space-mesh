import Foundation

/// 백그라운드 감시 모드 — 성격이 완전히 다른 세 가지 중 사용자가 택일.
enum WatchMode: String, CaseIterable, Identifiable {
    /// 상주·주기 실행 없음. 수동 스캔만. 부하 0.
    case off
    /// LaunchAgent가 저전력 조건에서 주기적으로 CLI를 실행해 스냅샷만 축적. 상주 없음.
    case periodic
    /// 앱 내 FSEvents 스트림으로 변경을 감지해 배치 재집계 + 버짓 알림. 상주.
    case live

    var id: String { rawValue }

    var title: String {
        switch self {
        case .off: return "끄기"
        case .periodic: return "주기 스냅샷"
        case .live: return "실시간 감시"
        }
    }

    var summary: String {
        switch self {
        case .off:
            return "백그라운드 작업 없음. 직접 스캔할 때만 동작합니다. 자원 소모 0."
        case .periodic:
            return "정해진 간격마다 조용히 스캔해 스냅샷을 쌓습니다 (저전력 IO, 배터리·발열 시 자동 지연). 상주하지 않아 부하가 거의 없고, '변화' 탭이 시간에 따라 채워집니다."
        case .live:
            return "파일 변경을 실시간 감지해 메뉴바에서 용량을 추적하고 예산 초과 시 알립니다. IO가 많을 때는 변경을 모아 유휴 시점에 한 번만 재집계합니다."
        }
    }
}

/// 주기 스냅샷 실행 간격.
enum PeriodicInterval: String, CaseIterable, Identifiable {
    case hourly, sixHourly, daily

    var id: String { rawValue }
    var seconds: Int {
        switch self {
        case .hourly: return 3600
        case .sixHourly: return 6 * 3600
        case .daily: return 24 * 3600
        }
    }
    var title: String {
        switch self {
        case .hourly: return "1시간마다"
        case .sixHourly: return "6시간마다"
        case .daily: return "하루 1회"
        }
    }
}

/// UserDefaults 기반 설정 저장소.
@MainActor
final class AppSettings: ObservableObject {
    static let shared = AppSettings()

    private let defaults = UserDefaults.standard

    @Published var mode: WatchMode {
        didSet { defaults.set(mode.rawValue, forKey: "watchMode") }
    }
    @Published var interval: PeriodicInterval {
        didSet { defaults.set(interval.rawValue, forKey: "periodicInterval") }
    }
    /// 감시 대상 루트 (마지막으로 스캔한 경로. 비어 있으면 홈).
    @Published var watchedRoot: String {
        didSet { defaults.set(watchedRoot, forKey: "watchedRoot") }
    }
    /// 실시간 모드 예산(GiB). 이 값을 넘으면 알림. 0이면 알림 없음.
    @Published var budgetGiB: Double {
        didSet { defaults.set(budgetGiB, forKey: "budgetGiB") }
    }

    // MARK: - 회수 제안 정책 (F6) — 계산·알림까지만, 자동 삭제 없음.

    /// 백그라운드가 회수 후보를 계산해 제안할지.
    @Published var suggestEnabled: Bool {
        didSet { defaults.set(suggestEnabled, forKey: "suggestEnabled") }
    }
    /// 이 일수 이상 유휴(git 마지막 커밋 기준)인 프로젝트의 산출물만 제안.
    @Published var suggestIdleDays: Int {
        didSet { defaults.set(suggestIdleDays, forKey: "suggestIdleDays") }
    }
    /// 제안 합계가 이 값(GiB) 미만이면 알리지 않는다 (소음 방지).
    @Published var suggestMinGiB: Double {
        didSet { defaults.set(suggestMinGiB, forKey: "suggestMinGiB") }
    }

    private init() {
        mode = WatchMode(rawValue: defaults.string(forKey: "watchMode") ?? "") ?? .off
        interval =
            PeriodicInterval(rawValue: defaults.string(forKey: "periodicInterval") ?? "") ?? .daily
        watchedRoot = defaults.string(forKey: "watchedRoot") ?? NSHomeDirectory()
        budgetGiB = defaults.double(forKey: "budgetGiB")
        suggestEnabled =
            defaults.object(forKey: "suggestEnabled") as? Bool ?? true
        suggestIdleDays = defaults.object(forKey: "suggestIdleDays") as? Int ?? 90
        suggestMinGiB = defaults.object(forKey: "suggestMinGiB") as? Double ?? 0.5
    }
}
