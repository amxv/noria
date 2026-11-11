import AppKit
import Foundation
import ServiceManagement

if CommandLine.arguments.contains("--version") {
    print("Noria " + (Bundle.main.object(forInfoDictionaryKey: "NoriaVersion") as? String ?? "unknown")); exit(0)
}
if CommandLine.arguments.contains("--register-login-item") || CommandLine.arguments.contains("--unregister-login-item") {
    do {
        if CommandLine.arguments.contains("--register-login-item") { try SMAppService.mainApp.register() }
        else { try SMAppService.mainApp.unregister() }
        exit(0)
    } catch { fputs("Login item could not be changed: \(error.localizedDescription)\n", stderr); exit(1) }
}
if CommandLine.arguments.contains("--login-item-status") { print(SMAppService.mainApp.status == .enabled ? "enabled" : "disabled"); exit(0) }
if CommandLine.arguments.contains("--check") {
    let file = cliURL()
    guard FileManager.default.isExecutableFile(atPath: file.path) else { fputs("Sibling noria CLI is missing\n", stderr); exit(1) }
    print("Native menu bar bundle and sibling CLI are ready"); exit(0)
}
if CommandLine.arguments.count == 3 && CommandLine.arguments[1] == "--validate-status-json" {
    do {
        let state = try decodeJSON(Dashboard.self, from: Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[2])))
        guard state.accounts.first?.title == "alex@example.com",
              let windows = state.accounts.first?.snapshot?.windows,
              windowText(windows.first, title: "5h").contains("75% left"),
              windowText(windows.last, title: "Week").contains("5% left") else { exit(1) }
        print("Native menu status decoder: emails, fractional timestamps, and quota percentages passed")
        exit(0)
    } catch { fputs("Invalid CLI status schema: \(error.localizedDescription)\n", stderr); exit(1) }
}
if CommandLine.arguments.count == 3 && CommandLine.arguments[1] == "--validate-compact-status-json" {
    do {
        let state = try decodeJSON(Dashboard.self, from: Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[2])))
        guard state.accounts.count == 1, visibleProviders(in: state.accounts) == ["codex"],
              let only = visibleQuotaWindows(for: state.accounts[0]).first,
              visibleQuotaWindows(for: state.accounts[0]).count == 1,
              only.title == "Week", windowText(only.window, title: only.title).contains("98% left")
        else { exit(1) }
        print("Native menu compact view: absent provider and unreported five-hour limit are hidden")
        exit(0)
    } catch { fputs("Invalid compact status fixture: \(error.localizedDescription)\n", stderr); exit(1) }
}
if CommandLine.arguments.count == 3 && CommandLine.arguments[1] == "--validate-fable-status-json" {
    do {
        let state = try decodeJSON(Dashboard.self, from: Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[2])))
        guard state.accounts.count == 3 else { exit(1) }
        let eligible = visibleQuotaWindows(for: state.accounts[0])
        let noFable = visibleQuotaWindows(for: state.accounts[1])
        let codex = visibleQuotaWindows(for: state.accounts[2])
        guard eligible.map(\.title) == ["5h", "Week", "Fable Week"],
              windowText(eligible[2].window, title: eligible[2].title).contains("100% left"),
              noFable.map(\.title) == ["Week"], codex.map(\.title) == ["Week"] else { exit(1) }
        print("Native menu Fable weekly quota: eligible, absent, and unrelated accounts passed")
        exit(0)
    } catch { fputs("Invalid Fable status schema: \(error.localizedDescription)\n", stderr); exit(1) }
}
if CommandLine.arguments.contains("--validate-quota-freshness") {
    let now = Date()
    func account(observed: Date, failedAt: Date? = nil) -> AccountStatus {
        AccountStatus(id: "fixture", email: "fixture@example.com", alias: nil,
            provider: "claude", enabled: true, inflight: 0,
            snapshot: QuotaSnapshot(observed_at: observed, available: true, auth_invalid: nil, windows: []),
            last_refresh: failedAt.map { QuotaRefreshStatus(attempted_at: $0, ok: false, error: "auth failed") })
    }
    guard quotaIsCurrent(for: account(observed: now), now: now),
          !quotaIsCurrent(for: account(observed: now.addingTimeInterval(-600)), now: now),
          !quotaIsCurrent(for: account(observed: now.addingTimeInterval(-45), failedAt: now), now: now)
    else { exit(1) }
    print("Native menu hides stale and failed quota data instead of presenting it as live")
    exit(0)
}
if CommandLine.arguments.count == 4 && CommandLine.arguments[1] == "--validate-refresh-settings" {
    do {
        let updated = try decodeJSON(Dashboard.self, from: Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[2])))
        let legacy = try decodeJSON(Dashboard.self, from: Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[3])))
        guard updated.preferences.quotaRefreshMinutes == 15,
              legacy.preferences.quotaRefreshMinutes == 0,
              quotaRefreshIntervals == [0, 2, 5, 10, 15, 30, 60, 120, 180],
              usageRefreshPeriod(minutes: 0) == nil,
              usageRefreshPeriod(minutes: 15) == 900,
              usageRefreshPeriod(minutes: 180) == 10800,
              usageRefreshPeriod(minutes: 3) == nil else { exit(1) }
        print("Native menu refresh preferences: legacy default, allowed intervals and disabled timer passed")
        exit(0)
    } catch { fputs("Invalid refresh preference fixture: \(error.localizedDescription)\n", stderr); exit(1) }
}
let home = selectedHome()
do { try FileManager.default.createDirectory(atPath: home, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700]) }
catch { fputs("Cannot open the Noria private home.\n", stderr); exit(1) }
guard let lock = SingleInstanceLock(home: home) else { exit(0) }
let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let delegate = AppDelegate(cli: CLI(executable: cliURL(), home: home))
app.delegate = delegate
withExtendedLifetime(lock) { app.run() }
