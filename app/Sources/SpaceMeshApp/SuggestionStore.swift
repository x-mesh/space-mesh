import Foundation
import SpaceMeshCore
import UserNotifications

/// 정책 기반 회수 제안 (F6). CLI `--suggest`가 쓰는 suggestions.json과 같은 스키마 —
/// 주기 모드는 파일로 전달받고, live 모드/수동 스캔은 in-process로 평가한다.
/// 어떤 경로든 *제안*까지만 — 삭제는 항상 사용자가 플랜 시트에서 확인한다.
struct SuggestionItem: Codable, Identifiable {
    var id: String { path }
    let path: String
    let title: String
    /// "rule" | "category"
    let source: String
    let safety: String
    let estimated: UInt64
    let recreateCommand: String
    let idleDays: UInt64?

    enum CodingKeys: String, CodingKey {
        case path, title, source, safety, estimated
        case recreateCommand = "recreate_command"
        case idleDays = "idle_days"
    }
}

struct Suggestion: Codable {
    let version: Int
    let generatedAt: UInt64
    let root: String
    let idleDays: UInt64
    let totalEstimated: UInt64
    let belowThreshold: Bool
    let items: [SuggestionItem]

    enum CodingKeys: String, CodingKey {
        case version, root, items
        case generatedAt = "generated_at"
        case idleDays = "idle_days"
        case totalEstimated = "total_estimated"
        case belowThreshold = "below_threshold"
    }
}

@MainActor
final class SuggestionStore: ObservableObject {
    static let shared = SuggestionStore()

    @Published var current: Suggestion?
    @Published var dismissed = false

    private var lastEvalAt: Date?
    private let defaults = UserDefaults.standard

    /// 주기 모드 CLI가 쓰는 산출물 경로 (스냅샷 DB와 같은 디렉토리).
    nonisolated static var filePath: String {
        (AppModel.dbPath as NSString)
            .deletingLastPathComponent + "/suggestions.json"
    }

    /// 파일에서 로드 — 앱 시작 시 호출. 7일 넘었거나 비어 있으면 무시.
    func loadFromDisk() {
        guard let data = FileManager.default.contents(atPath: Self.filePath),
            let suggestion = try? JSONDecoder().decode(Suggestion.self, from: data)
        else { return }
        let age = Date().timeIntervalSince1970 - Double(suggestion.generatedAt)
        guard age < 7 * 86_400, !suggestion.items.isEmpty else { return }
        current = suggestion
        dismissed = false
    }

    /// 스캔 핸들로 in-process 평가 — live 모드 재집계 후/수동 스캔 후 호출.
    /// 룰 경로 측정이 가벼운 스캔을 동반하므로 6시간에 한 번만 실제 평가한다.
    func evaluate(handle: ScanHandle, root: String, settings: AppSettings) async -> Suggestion? {
        guard settings.suggestEnabled else { return nil }
        if let last = lastEvalAt, Date().timeIntervalSince(last) < 6 * 3600 {
            return current
        }
        lastEvalAt = Date()

        let idleDays = UInt64(max(0, settings.suggestIdleDays))
        let minBytes = UInt64(max(0, settings.suggestMinGiB) * 1_073_741_824)
        let (candidates, hits) = await Task.detached(priority: .utility) {
            (detectCleanup(home: NSHomeDirectory()), handle.categories())
        }.value

        var items: [SuggestionItem] = []
        for c in candidates where c.safety == "safe" {
            items.append(
                SuggestionItem(
                    path: c.path, title: c.title, source: "rule", safety: c.safety,
                    estimated: c.allocatedSize, recreateCommand: c.recreateCommand,
                    idleDays: nil))
        }
        // 유휴 판정이 불가능한(git 없는) 프로젝트는 보수적으로 제외.
        for h in hits where h.verified && h.safety == "safe" && (h.idleDays ?? 0) >= idleDays {
            items.append(
                SuggestionItem(
                    path: h.path, title: h.title, source: "category", safety: h.safety,
                    estimated: h.allocatedSize, recreateCommand: h.recreateCommand,
                    idleDays: h.idleDays))
        }
        let total = items.reduce(UInt64(0)) { $0 + $1.estimated }
        guard total >= minBytes, !items.isEmpty else { return nil }

        let suggestion = Suggestion(
            version: 1,
            generatedAt: UInt64(Date().timeIntervalSince1970),
            root: root,
            idleDays: idleDays,
            totalEstimated: total,
            belowThreshold: false,
            items: items)
        current = suggestion
        dismissed = false
        return suggestion
    }

    /// 알림 — 같은 제안 집합은 재알림하지 않고, 다른 집합이라도 24시간에 1회.
    func notifyIfNew(_ suggestion: Suggestion) {
        let key = suggestion.items.map(\.path).sorted().joined(separator: "|")
        let lastKey = defaults.string(forKey: "suggestNotifiedKey")
        let lastAt = defaults.double(forKey: "suggestNotifiedAt")
        guard key != lastKey,
            Date().timeIntervalSince1970 - lastAt > 24 * 3600
        else { return }
        defaults.set(key, forKey: "suggestNotifiedKey")
        defaults.set(Date().timeIntervalSince1970, forKey: "suggestNotifiedAt")

        let content = UNMutableNotificationContent()
        content.title = "회수 가능한 공간 발견"
        content.body =
            "\(suggestion.items.count)개 항목 · \(humanBytes(suggestion.totalEstimated)) — 열어서 검토하세요 (자동 삭제 없음)"
        content.sound = nil  // 파괴적이지 않은 정보 — 조용하게 (원칙 4)
        let req = UNNotificationRequest(
            identifier: "suggest-\(suggestion.generatedAt)", content: content, trigger: nil)
        UNUserNotificationCenter.current().add(req)
    }
}
