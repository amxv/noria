import Foundation

struct QuotaWindow: Decodable {
    let name: String
    let model: String?
    let used_percent: Double?
    let resets_at: Date?
    let duration: Int64?
}
struct QuotaSnapshot: Decodable {
    let observed_at: Date
    let available: Bool
    let auth_invalid: Bool?
    let windows: [QuotaWindow]?
}
struct QuotaRefreshStatus: Decodable {
    let attempted_at: Date
    let ok: Bool
    let error: String?
}
struct QuotaRefreshResult: Decodable {
    let account: String
    let ok: Bool
    let error: String?
}
struct AccountStatus: Decodable {
    let id: String
    let email: String?
    let alias: String?
    let provider: String
    let enabled: Bool
    let inflight: Int?
    let snapshot: QuotaSnapshot?
    let last_refresh: QuotaRefreshStatus?
    var title: String { if let alias, !alias.isEmpty { return alias }; if let email, !email.isEmpty { return email }; return "Sign-in needed" }
}

// A last-known percentage must never be presented as a live quota if the
// read has failed or the snapshot is older than the polling interval.
func quotaIsCurrent(for account: AccountStatus, now: Date = Date()) -> Bool {
    guard let snapshot = account.snapshot, snapshot.available else { return false }
    let age = now.timeIntervalSince(snapshot.observed_at)
    guard age >= -30 && age <= 180 else { return false }
    if let refresh = account.last_refresh,
       !refresh.ok && refresh.attempted_at >= snapshot.observed_at { return false }
    return true
}
struct Preferences: Decodable {
    let start_at_login: Bool
    let menubar_at_login: Bool
    let auto_update: Bool
    // Older CLI status fixtures and installations omit this field. Zero
    // means refresh only when the user opens the menu or explicitly requests.
    let quota_refresh_minutes: Int?
    var quotaRefreshMinutes: Int { quota_refresh_minutes ?? 0 }
}
let quotaRefreshIntervals = [0, 2, 5, 10, 15, 30, 60, 120, 180]
func usageRefreshPeriod(minutes: Int) -> TimeInterval? {
    guard minutes > 0, quotaRefreshIntervals.contains(minutes) else { return nil }
    return TimeInterval(minutes * 60)
}
struct ClientRoot: Decodable { let provider: String; let root: String }
struct ClientIntegration: Decodable {
    let provider: String
    let path: String
    let active: Bool
    let healthy: Bool
}
struct Dashboard: Decodable {
    let schema_version: Int
    let version: String
    let running: Bool
    let accounts: [AccountStatus]
    let preferences: Preferences
    let client_roots: [ClientRoot]
    let integrations: [ClientIntegration]
}

// Only draw quota windows the provider actually reports with a usable
// percentage. The Fable weekly bucket is optional per account (including
// promotions), so never infer it from the subscription plan.
func visibleQuotaWindows(for account: AccountStatus) -> [(title: String, window: QuotaWindow)] {
    let windows = account.snapshot?.windows ?? []
    var targets: [(duration: Int64, title: String, model: String)] = [
        (18_000_000_000_000, "5h", ""),
        (604_800_000_000_000, "Week", "")
    ]
    if account.provider == "claude" { targets.append((604_800_000_000_000, "Fable Week", "fable")) }
    return targets.compactMap { duration, title, model in
        guard let window = windows.first(where: {
            $0.duration == duration && ($0.model ?? "") == model &&
            (model != "fable" || $0.name == "seven_day_fable") &&
            $0.used_percent.map { $0.isFinite && $0 >= 0 && $0 <= 100 } == true
        }) else { return nil }
        return (title: title, window: window)
    }
}

// Avoid empty provider headings. The main menu should represent the actual
// pool instead of advertising providers the user hasn't added yet.
func visibleProviders(in accounts: [AccountStatus]) -> [String] {
    ["codex", "claude"].filter { provider in
        accounts.contains { $0.provider == provider }
    }
}
struct UpdateResult: Decodable {
    let current: String
    let latest: String
    let available: Bool
    let updated: Bool
    let deferred: Bool
    let message: String
}
func decodeJSON<T: Decodable>(_ type: T.Type, from data: Data) throws -> T {
    let decoder = JSONDecoder()
    decoder.dateDecodingStrategy = .custom { decoder in
        let value = try decoder.singleValueContainer().decode(String.self)
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = formatter.date(from: value) { return date }
        formatter.formatOptions = [.withInternetDateTime]
        if let date = formatter.date(from: value) { return date }
        throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "Invalid timestamp"))
    }
    return try decoder.decode(type, from: data)
}
func windowText(_ window: QuotaWindow?, title: String) -> String {
    guard let window else { return "\(title)    Usage unavailable" }
    let left = window.used_percent.map { String(format: "%.0f%% left", max(0, min(100, 100 - $0))) } ?? "Unknown"
    guard let reset = window.resets_at else { return "\(title)    \(left) · reset unknown" }
    let seconds = reset.timeIntervalSinceNow
    if seconds <= 0 { return "\(title)    \(left) · refreshing reset" }
    let minutes = Int(ceil(seconds / 60))
    let countdown: String
    if minutes >= 1440 { countdown = "\(minutes / 1440)d \((minutes % 1440) / 60)h" }
    else if minutes >= 60 { countdown = "\(minutes / 60)h \(minutes % 60)m" }
    else { countdown = "\(minutes)m" }
    return "\(title)    \(left) · resets in \(countdown)"
}
