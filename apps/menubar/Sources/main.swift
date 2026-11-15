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
if CommandLine.arguments.contains("--validate-quota-levels") {
    let now = Date()
    let window = QuotaWindow(name: "five_hour", model: nil, used_percent: 16, resets_at: now.addingTimeInterval(9_660), duration: 18_000_000_000_000)
    let row = QuotaRowModel(title: "5h", window: window, now: now)
    guard QuotaLevel(percentLeft: 84) == .healthy, QuotaLevel(percentLeft: 25) == .healthy,
          QuotaLevel(percentLeft: 24.9) == .low, QuotaLevel(percentLeft: 10) == .low,
          QuotaLevel(percentLeft: 9.9) == .critical, QuotaLevel(percentLeft: 0) == .critical,
          row.percentText == "84% left", row.resetText == "Resets in 2h 41m", row.level == .healthy,
          QuotaRowModel(title: "Week", window: QuotaWindow(name: "seven_day", model: nil, used_percent: nil, resets_at: nil, duration: nil), now: now).level == nil
    else { exit(1) }
    print("Native menu quota bars: green, yellow below 25% and red below 10% left")
    exit(0)
}
// Development aid: draw sample quota rows in light and dark appearance.
if CommandLine.arguments.count == 3 && CommandLine.arguments[1] == "--render-quota-preview" {
    let now = Date()
    let samples: [(String, Double, TimeInterval)] = [("5h", 16, 9_660), ("Week", 76, 35_460), ("Fable Week", 59, 35_460), ("5h", 93, 1_500)]
    let width: CGFloat = 300, rowHeight = QuotaRowView.height
    let column = NSView(frame: NSRect(x: 0, y: 0, width: width * 2, height: rowHeight * CGFloat(samples.count)))
    for (side, name) in [NSAppearance.Name.aqua, .darkAqua].enumerated() {
        let pane = NSView(frame: NSRect(x: CGFloat(side) * width, y: 0, width: width, height: column.frame.height))
        pane.appearance = NSAppearance(named: name)
        pane.wantsLayer = true
        pane.layer?.backgroundColor = (name == .aqua ? NSColor(white: 0.96, alpha: 1) : NSColor(white: 0.16, alpha: 1)).cgColor
        for (index, sample) in samples.enumerated() {
            let window = QuotaWindow(name: sample.0, model: nil, used_percent: sample.1, resets_at: now.addingTimeInterval(sample.2), duration: nil)
            let view = QuotaRowView(model: QuotaRowModel(title: sample.0, window: window, now: now), width: width)
            view.frame.origin.y = CGFloat(samples.count - 1 - index) * rowHeight
            pane.addSubview(view)
        }
        column.addSubview(pane)
    }
    guard let bitmap = column.bitmapImageRepForCachingDisplay(in: column.bounds) else { exit(1) }
    column.cacheDisplay(in: column.bounds, to: bitmap)
    guard let png = bitmap.representation(using: .png, properties: [:]),
          (try? png.write(to: URL(fileURLWithPath: CommandLine.arguments[2]))) != nil else { exit(1) }
    exit(0)
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
