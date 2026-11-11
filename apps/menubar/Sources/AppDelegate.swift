import AppKit
import Darwin
import Foundation

// Native presentation only: every mutation and provider operation is a Go CLI command.
final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate {
    private let cli: CLI
    private let menu = NSMenu()
    private var statusItem: NSStatusItem!
    private var dashboard: Dashboard?
    private var refreshing = false
    private var statusInFlight = false
    private var mutationInFlight = false
    private var loginProcess: Process?
    private var updateInFlight = false
    private var relaunchInFlight = false
    private var updateAgentError: String?
    private var updateTimer: Timer?
    private var usageTimer: Timer?
    private var configuredUsageRefreshMinutes: Int?
    private final class ActionBox: NSObject { let action: () -> Void; init(_ action: @escaping () -> Void) { self.action = action } }
    private var refreshMessage: String?
    init(cli: CLI) { self.cli = cli; super.init() }

    func applicationDidFinishLaunching(_ notification: Notification) {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        statusItem.button?.title = "N"
        statusItem.button?.font = .systemFont(ofSize: 14, weight: .semibold)
        statusItem.button?.toolTip = "Noria — your local account router"
        menu.delegate = self
        menu.autoenablesItems = false
        statusItem.menu = menu
        render()
        cli.run(["init"], timeout: 10) { [weak self] _ in
            self?.loadStatus()
            self?.syncUpdateAgent()
        }
        updateTimer = Timer.scheduledTimer(withTimeInterval: 3600, repeats: true) { [weak self] _ in self?.checkUpdate(automatic: true) }
        DispatchQueue.main.asyncAfter(deadline: .now() + 30) { [weak self] in self?.checkUpdate(automatic: true) }
    }
    func applicationWillTerminate(_ notification: Notification) { updateTimer?.invalidate(); usageTimer?.invalidate(); if loginProcess?.isRunning == true { loginProcess?.terminate() } }
    func menuWillOpen(_ menu: NSMenu) { loadStatus(); refreshUsage() }

    private func item(_ title: String, enabled: Bool = true, check: Bool? = nil, symbol: String? = nil, action: (() -> Void)? = nil) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action == nil ? nil : #selector(performAction(_:)), keyEquivalent: "")
        item.target = self; item.isEnabled = enabled
        if let check { item.state = check ? .on : .off }
        if let symbol { item.image = NSImage(systemSymbolName: symbol, accessibilityDescription: title); item.image?.isTemplate = true }
        if let action { item.representedObject = ActionBox(action) }
        return item
    }
    @objc private func performAction(_ sender: NSMenuItem) { (sender.representedObject as? ActionBox)?.action() }
    private func label(_ text: String) -> NSMenuItem { item(text, enabled: false) }
    private func render() {
        menu.removeAllItems()
        let running = dashboard?.running ?? false
        statusItem.button?.appearsDisabled = dashboard != nil && !running
        menu.addItem(label(running ? "Noria is Running" : "Noria is Stopped"))
        menu.addItem(item(running ? "Stop Noria" : "Start Noria", enabled: !mutationInFlight, symbol: running ? "stop.fill" : "play.fill") { [weak self] in self?.changeService(running ? "stop" : "start") })
        if mutationInFlight { menu.addItem(label("Applying change…")) }
        let accounts = dashboard?.accounts ?? []
        if !accounts.isEmpty { menu.addItem(.separator()) }
        for provider in visibleProviders(in: accounts) {
            let providerAccounts = accounts.filter { $0.provider == provider }
            menu.addItem(label(provider == "codex" ? "CODEX ACCOUNTS" : "CLAUDE ACCOUNTS"))
            for account in providerAccounts { addAccount(account) }
            menu.addItem(.separator())
        }
        menu.addItem(item(refreshing ? "Refreshing Usage…" : "Refresh Usage", enabled: !refreshing, symbol: "arrow.clockwise") { [weak self] in self?.refreshUsage() })
        if let refreshMessage { menu.addItem(label(refreshMessage)) }
        menu.addItem(.separator())
        let configureMenu = NSMenu(); configureMenu.autoenablesItems = false
        for provider in ["codex", "claude"] {
            let roots = (dashboard?.client_roots ?? []).filter { $0.provider == provider }
            let name = provider == "codex" ? "Codex" : "Claude Code"
            if roots.count == 1, let root = roots.first {
                let action = item("Configure \(name)") { [weak self] in self?.configure(root: root, remove: false) }
                action.toolTip = (root.root as NSString).abbreviatingWithTildeInPath
                configureMenu.addItem(action)
            } else if !roots.isEmpty {
                let rootMenu = NSMenu(); rootMenu.autoenablesItems = false
                for root in roots {
                    let path = (root.root as NSString).abbreviatingWithTildeInPath
                    rootMenu.addItem(item(path) { [weak self] in self?.configure(root: root, remove: false) })
                }
                let providerItem = item("Configure \(name)")
                providerItem.submenu = rootMenu
                configureMenu.addItem(providerItem)
            }
        }
        configureMenu.addItem(item("Claude Desktop Setup…") { NSWorkspace.shared.open(URL(string: "https://noria.ashray.xyz/docs/native-clients/#what-about-claude-desktop")!) })
        configureMenu.addItem(item("Choose a Custom Client Folder…") { [weak self] in self?.customIntegration() })
        for entry in dashboard?.integrations ?? [] {
            let path = (entry.path as NSString).abbreviatingWithTildeInPath
            configureMenu.addItem(item("Restore Direct Settings: \(path)…") { [weak self] in self?.configure(root: ClientRoot(provider: entry.provider, root: (entry.path as NSString).deletingLastPathComponent), remove: true) })
        }
        configureMenu.addItem(.separator())
        configureMenu.addItem(item("Add Codex Account…", enabled: loginProcess == nil, symbol: "person.badge.plus") { [weak self] in self?.authenticate(provider: "codex", account: nil) })
        configureMenu.addItem(item("Add Claude Account…", enabled: loginProcess == nil, symbol: "person.badge.plus") { [weak self] in self?.authenticate(provider: "claude", account: nil) })
        if loginProcess != nil {
            configureMenu.addItem(label("Finish sign-in in your browser…"))
            configureMenu.addItem(item("Cancel Sign-In") { [weak self] in self?.loginProcess?.terminate() })
        }
        configureMenu.addItem(item("Health Check", symbol: "heart.text.square") { [weak self] in self?.healthCheck() })
        configureMenu.addItem(.separator())
        let prefs = dashboard?.preferences
        configureMenu.addItem(item("Launch Menu Bar at Login", enabled: !mutationInFlight, check: prefs?.menubar_at_login ?? false) { [weak self] in self?.setPreference("menubar-at-login", enabled: !(prefs?.menubar_at_login ?? false)) })
        configureMenu.addItem(item("Start Noria at Login", enabled: !mutationInFlight, check: prefs?.start_at_login ?? false) { [weak self] in self?.setPreference("start-at-login", enabled: !(prefs?.start_at_login ?? false)) })
        let refreshMenu = NSMenu(); refreshMenu.autoenablesItems = false
        for minutes in quotaRefreshIntervals {
            let title = minutes == 0 ? "On Menu Open Only (Default)" : "Every \(minutes) Minutes"
            refreshMenu.addItem(item(title, enabled: !mutationInFlight,
                check: (prefs?.quotaRefreshMinutes ?? 0) == minutes) { [weak self] in
                self?.mutate(["preferences", "quota-refresh-minutes", String(minutes), "--json"])
            })
        }
        let refreshSetting = item("Usage Auto-Refresh")
        refreshSetting.submenu = refreshMenu
        configureMenu.addItem(refreshSetting)
        configureMenu.addItem(item("Automatically Update", enabled: !mutationInFlight, check: prefs?.auto_update ?? false) { [weak self] in self?.setPreference("auto-update", enabled: !(prefs?.auto_update ?? false)) })
        if updateAgentError != nil && prefs?.auto_update == true {
            configureMenu.addItem(item("Repair Automatic Updates…") { [weak self] in self?.syncUpdateAgent() })
        }
        configureMenu.addItem(item(updateInFlight ? "Checking for Updates…" : "Check for Updates…", enabled: !updateInFlight) { [weak self] in self?.checkUpdate(automatic: false) })
        configureMenu.addItem(item("Open Documentation", symbol: "book") { NSWorkspace.shared.open(URL(string: "https://noria.ashray.xyz/docs/quickstart/")!) })
        configureMenu.addItem(.separator())
        let version = Bundle.main.object(forInfoDictionaryKey: "NoriaVersion") as? String ?? "unknown"
        configureMenu.addItem(label("Noria \(dashboard?.version ?? version)"))
        let configureItem = item("Configure", symbol: "gearshape")
        configureItem.submenu = configureMenu
        menu.addItem(configureItem)
        menu.addItem(.separator())
        menu.addItem(item("Quit Menu Bar (Keep Noria Running)") { NSApplication.shared.terminate(nil) })
    }
    private func addAccount(_ account: AccountStatus) {
        let header = item(account.title + (account.enabled ? "" : " · Disabled"), symbol: account.enabled ? "person.crop.circle" : "person.crop.circle.badge.minus")
        header.toolTip = account.email
        let commands = NSMenu(); commands.autoenablesItems = false
        if let email = account.email, !email.isEmpty { commands.addItem(label(email)) }
        commands.addItem(item(account.enabled ? "Disable Account" : "Enable Account", enabled: !mutationInFlight) { [weak self] in self?.mutate(["account", account.enabled ? "disable" : "enable", account.id, "--json"]) })
        commands.addItem(item("Sign In Again…", enabled: loginProcess == nil) { [weak self] in self?.authenticate(provider: account.provider, account: account) })
        commands.addItem(item("Rename / Set Alias…") { [weak self] in self?.rename(account) })
        if let alias = account.alias, !alias.isEmpty { commands.addItem(item("Use Email as Name") { [weak self] in self?.mutate(["account", "rename", account.id, "--json"]) }) }
        commands.addItem(item("Remove from Pool…") { [weak self] in
            guard let self else { return }
            if self.confirm("Remove \(account.title)?", "This unregisters the account. Its isolated sign-in files are retained; your normal client login is unchanged.", button: "Remove") { self.mutate(["account", "remove", account.id, "--json"]) }
        })
        header.submenu = commands; menu.addItem(header)
        let current = quotaIsCurrent(for: account)
        let rows = current ? visibleQuotaWindows(for: account) : []
        for (title, window) in rows {
            let row = label(windowText(window, title: title)); row.indentationLevel = 1
            if let reset = window.resets_at { let format = DateFormatter(); format.dateStyle = .medium; format.timeStyle = .short; row.toolTip = "Resets \(format.string(from: reset))" }
            menu.addItem(row)
        }
        if rows.isEmpty {
            let unavailable = label(current ? "Usage unavailable" : (refreshing ? "Checking live usage…" : "Live usage unavailable")); unavailable.indentationLevel = 1
            menu.addItem(unavailable)
        }
        if let last = account.last_refresh, !last.ok {
            let error = last.error ?? "Quota provider unavailable"
            let failure = label("Refresh failed · \(String(error.prefix(75)))")
            failure.toolTip = error
            failure.indentationLevel = 1
            menu.addItem(failure)
        } else if !current, let observed = account.snapshot?.observed_at {
            let format = DateFormatter(); format.dateStyle = .none; format.timeStyle = .short
            let stale = label("Last verified \(format.string(from: observed)) · not live"); stale.indentationLevel = 1
            menu.addItem(stale)
        }
    }
    private func loadStatus() {
        guard !statusInFlight else { return }; statusInFlight = true
        cli.run(["status", "--json"], timeout: 8) { [weak self] result in
            guard let self else { return }; self.statusInFlight = false
            if result.status == 0, let state = try? decodeJSON(Dashboard.self, from: result.output), state.schema_version == 1 { self.dashboard = state
                self.configureUsageTimer(minutes: state.preferences.quotaRefreshMinutes)
                if let plistData = try? Data(contentsOf: Bundle.main.bundleURL.appendingPathComponent("Contents/Info.plist")),
                   let plist = (try? PropertyListSerialization.propertyList(from: plistData, options: [], format: nil)) as? [String: Any],
                   let installed = plist["NoriaVersion"] as? String,
                   let loaded = Bundle.main.object(forInfoDictionaryKey: "NoriaVersion") as? String,
                   installed == state.version, installed != loaded, !self.updateInFlight { self.restartMenu(); return }
            }
            self.render()
        }
    }
    // The CLI is the only settings store. An interval change picked up from
    // status immediately invalidates the previous timer; zero disables it.
    private func configureUsageTimer(minutes: Int) {
        guard configuredUsageRefreshMinutes != minutes else { return }
        configuredUsageRefreshMinutes = minutes
        usageTimer?.invalidate()
        usageTimer = nil
        guard let seconds = usageRefreshPeriod(minutes: minutes) else { return }
        usageTimer = Timer.scheduledTimer(withTimeInterval: seconds, repeats: true) { [weak self] _ in
            self?.refreshUsage()
        }
    }
    private func refreshUsage() {
        guard !refreshing else { return }; refreshing = true; refreshMessage = nil; render()
        cli.run(["quota", "refresh"], timeout: 125) { [weak self] result in
            guard let self else { return }; self.refreshing = false
            if result.status != 0 {
                if let outcomes = try? decodeJSON([QuotaRefreshResult].self, from: result.output),
                   let failure = outcomes.first(where: { !$0.ok }) {
                    self.refreshMessage = "Refresh failed · \(String((failure.error ?? "Unknown error").prefix(90)))"
                } else {
                    self.refreshMessage = "Refresh failed · \(String(result.error.prefix(90)))"
                }
            } else { self.refreshMessage = nil }
            self.loadStatus(); self.render()
        }
    }
    private func mutate(_ arguments: [String]) {
        guard !mutationInFlight else { return }; mutationInFlight = true; render()
        cli.run(arguments, timeout: 40) { [weak self] result in
            guard let self else { return }; self.mutationInFlight = false
            if result.status != 0 { self.alert("Noria could not apply the change", result.error) }
            self.loadStatus(); self.render()
        }
    }
    private func changeService(_ action: String) { mutate([action, "--json"]) }
    private func setPreference(_ key: String, enabled: Bool) {
        if key == "auto-update" { updateAgentError = nil }
        mutate(["preferences", key, enabled ? "on" : "off", "--json"])
    }
    private func syncUpdateAgent() {
        cli.run(["__sync-updates"], timeout: 20) { [weak self] result in
            guard let self else { return }
            self.updateAgentError = result.status == 0 ? nil : result.error
            self.render()
        }
    }
    private func authenticate(provider: String, account: AccountStatus?) {
        guard loginProcess == nil else { return }
        let arguments = account.map { ["account", "relogin", $0.id, "--json"] } ?? ["account", "add", provider, "--json"]
        loginProcess = cli.run(arguments, timeout: 310) { [weak self] result in
            guard let self else { return }; self.loginProcess = nil
            if result.status != 0 { self.alert("Sign-in did not complete", result.error) }
            self.loadStatus(); self.refreshUsage(); self.render()
        }
        render()
    }
    private func rename(_ account: AccountStatus) {
        let alert = NSAlert(); alert.messageText = "Name for \(account.email ?? account.title)"; alert.informativeText = "Leave this blank to display the account email."
        alert.addButton(withTitle: "Save"); alert.addButton(withTitle: "Cancel")
        let input = NSTextField(string: account.alias ?? ""); input.frame = NSRect(x: 0, y: 0, width: 300, height: 24); alert.accessoryView = input
        NSApplication.shared.activate(ignoringOtherApps: true)
        if alert.runModal() == .alertFirstButtonReturn { mutate(["account", "rename", account.id, input.stringValue, "--json"]) }
    }
    private func configure(root: ClientRoot, remove: Bool) {
        let action = remove ? "Restore direct settings" : "Configure \(root.provider) for Noria"
        let message = "\(root.root)\n\nOnly routing settings change. Your login, conversation history and other preferences stay intact. Reopen already-running clients after this change."
        guard confirm(action + "?", message, button: remove ? "Restore" : "Configure") else { return }
        var arguments = ["integrate", root.provider, "--client-home", root.root, "--yes", "--json"]
        if remove { arguments.append("--remove") }
        mutate(arguments)
    }
    private func customIntegration() {
        let panel = NSOpenPanel(); panel.title = "Choose the Codex or Claude Code configuration folder"; panel.canChooseFiles = false; panel.canChooseDirectories = true; panel.allowsMultipleSelection = false
        NSApplication.shared.activate(ignoringOtherApps: true)
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let alert = NSAlert(); alert.messageText = "Which client uses this folder?"; alert.informativeText = url.path
        alert.addButton(withTitle: "Codex"); alert.addButton(withTitle: "Claude Code"); alert.addButton(withTitle: "Cancel")
        let result = alert.runModal(); if result == .alertFirstButtonReturn { configure(root: ClientRoot(provider: "codex", root: url.path), remove: false) }
        else if result == .alertSecondButtonReturn { configure(root: ClientRoot(provider: "claude", root: url.path), remove: false) }
    }
    private func healthCheck() {
        cli.run(["health", "--json"], timeout: 20) { [weak self] result in
            guard let self else { return }
            if result.status != 0 { self.alert("Health Check", result.error); return }
            let report = (try? JSONSerialization.jsonObject(with: result.output)) as? [String: Any]
            let checks = report?["checks"] as? [[String: Any]] ?? []
            let text = checks.map { "\(($0["ok"] as? Bool ?? false) ? "✓" : "•") \($0["name"] as? String ?? "Check")\n\($0["detail"] as? String ?? "")" }.joined(separator: "\n\n")
            self.alert("Noria Health Check", text + "\n\nNo model request was sent.")
        }
    }
    private func checkUpdate(automatic: Bool) {
        guard !updateInFlight, loginProcess == nil, !mutationInFlight else { return }; updateInFlight = true; render()
        let arguments = automatic ? ["upgrade", "--automatic", "--json"] : ["upgrade", "--check", "--json"]
        cli.run(arguments, timeout: 185) { [weak self] result in
            guard let self else { return }; self.updateInFlight = false
            guard result.status == 0 else { if !automatic { self.alert("Update check failed", result.error) }; self.render(); return }
            guard let update = try? decodeJSON(UpdateResult.self, from: result.output) else { self.render(); return }
            if update.updated { self.restartMenu(); return }
            if !automatic {
                if update.available {
                    if self.confirm("Noria \(update.latest) is available", "The CLI verifies the download. Updates wait until requests and live streaming connections are idle.", button: "Update") { self.applyUpdate() }
                } else { self.alert("Noria is up to date", "Version \(update.current)") }
            }
            self.render()
        }
    }
    private func applyUpdate() {
        updateInFlight = true; render()
        cli.run(["upgrade", "--json"], timeout: 185) { [weak self] result in
            guard let self else { return }; self.updateInFlight = false
            if result.status != 0 { self.alert("Update failed", result.error) }
            else if let update = try? decodeJSON(UpdateResult.self, from: result.output) {
                if update.updated { self.restartMenu(); return }
                self.alert("Noria Update", update.message)
            }
            self.render()
        }
    }
    private func restartMenu() {
        guard !relaunchInFlight else { return }
        relaunchInFlight = true
        cli.run(["menubar", "reopen", "--wait-pid", String(getpid())], timeout: 5) { [weak self] result in
            if result.status == 0 { NSApplication.shared.terminate(nil) }
            else { self?.relaunchInFlight = false; self?.alert("Could not reopen Noria", result.error) }
        }
    }
    private func confirm(_ title: String, _ text: String, button: String) -> Bool {
        let alert = NSAlert(); alert.messageText = title; alert.informativeText = text; alert.addButton(withTitle: button); alert.addButton(withTitle: "Cancel")
        NSApplication.shared.activate(ignoringOtherApps: true); return alert.runModal() == .alertFirstButtonReturn
    }
    private func alert(_ title: String, _ text: String) {
        let alert = NSAlert(); alert.messageText = title; alert.informativeText = String(text.prefix(1600)); alert.addButton(withTitle: "OK")
        NSApplication.shared.activate(ignoringOtherApps: true); alert.runModal()
    }
}
