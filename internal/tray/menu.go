//go:build windows || linux

package tray

import (
	"fmt"
	"path/filepath"
	"time"

	"github.com/amxv/noria/internal/config"
	"github.com/gogpu/systray"
)

func disabled(menu *systray.Menu, title string) {
	menu.Add(title, nil).SetDisabled(true)
}

func addAction(menu *systray.Menu, title string, enabled bool, action func()) {
	item := menu.Add(title, action)
	item.SetDisabled(!enabled)
}

func addCheck(menu *systray.Menu, title string, value bool, enabled bool, action func()) {
	item := menu.AddCheckbox(title, value, action)
	item.SetDisabled(!enabled)
}

// All menu callbacks invoke CLI-owned operations. No account, preference or
// service state is cached in a second settings store.
func (a *app) buildMenu() *systray.Menu {
	menu := systray.NewMenu()
	d := a.dashboard
	running := d != nil && d.Running
	state := "Noria is Stopped"
	if d == nil {
		state = "Loading Noria…"
	} else if running {
		state = "Noria is Running"
	}
	disabled(menu, state)
	startAction := "Start Noria"
	if running {
		startAction = "Stop Noria"
	}
	addAction(menu, startAction, !a.mutationBusy && d != nil, func() {
		command := "start"
		if running {
			command = "stop"
		}
		a.mutation(command, "--json")
	})
	if a.mutationBusy {
		disabled(menu, "Applying change…")
	}

	if d != nil {
		for _, provider := range d.Providers() {
			menu.AddSeparator()
			heading := "CODEX ACCOUNTS"
			if provider == "claude" {
				heading = "CLAUDE ACCOUNTS"
			}
			disabled(menu, heading)
			for _, account := range d.Accounts {
				if account.Provider == provider {
					a.addAccount(menu, account)
				}
			}
		}
	}
	menu.AddSeparator()
	label := "Refresh Usage"
	if a.quotaBusy {
		label = "Refreshing Usage…"
	}
	addAction(menu, label, !a.quotaBusy, func() { a.refreshQuota() })
	if a.refreshMessage != "" {
		disabled(menu, a.refreshMessage)
	}
	menu.AddSeparator()
	a.addConfigure(menu, d)
	menu.AddSeparator()
	addAction(menu, "Quit Tray App (Keep Noria Running)", true, func() { a.quit() })
	return menu
}

func (a *app) addAccount(menu *systray.Menu, account Account) {
	name := account.Title()
	if !account.Enabled {
		name += " · Disabled"
	}
	submenu := systray.NewMenu()
	if account.Email != "" {
		disabled(submenu, account.Email)
	}
	enabled := a.loginCancel == nil && !a.mutationBusy
	action := "Disable Account"
	command := "disable"
	if !account.Enabled {
		action, command = "Enable Account", "enable"
	}
	addAction(submenu, action, !a.mutationBusy, func() { a.mutation("account", command, account.ID, "--json") })
	addAction(submenu, "Sign In Again…", enabled, func() { a.authenticate(account.Provider, account.ID) })
	addAction(submenu, "Rename / Set Alias…", !a.mutationBusy, func() {
		go func() {
			newName, ok, err := prompt("Name for "+account.Title()+" (blank uses email)", account.Alias)
			if err != nil {
				a.errorNotice("Alias dialog unavailable", err)
				return
			}
			if ok {
				a.mutation("account", "rename", account.ID, newName, "--json")
			}
		}()
	})
	if account.Alias != "" {
		addAction(submenu, "Use Email as Name", !a.mutationBusy, func() { a.mutation("account", "rename", account.ID, "--json") })
	}
	addAction(submenu, "Remove from Pool…", !a.mutationBusy, func() {
		go func() {
			ok, err := confirm("Remove "+account.Title()+"?", "This unregisters the account. Its isolated sign-in files are retained; your normal client login is unchanged.")
			if err != nil {
				a.errorNotice("Confirmation unavailable", err)
				return
			}
			if ok {
				a.mutation("account", "remove", account.ID, "--json")
			}
		}()
	})
	menu.AddSubmenu(name, submenu)
	rows := account.CurrentWindows(time.Now())
	for _, row := range rows {
		disabled(menu, "    "+row.Text(time.Now()))
	}
	if len(rows) == 0 {
		if a.quotaBusy {
			disabled(menu, "    Checking live usage…")
		} else if account.QuotaIsCurrent(time.Now()) {
			disabled(menu, "    Usage unavailable")
		} else {
			disabled(menu, "    Live usage unavailable")
		}
	}
	if account.LastRefresh != nil && !account.LastRefresh.OK {
		disabled(menu, "    Refresh failed · "+account.LastRefresh.Error)
	} else if !account.QuotaIsCurrent(time.Now()) && account.Snapshot != nil {
		disabled(menu, "    Last verified "+account.Snapshot.ObservedAt.Local().Format("3:04 PM")+" · not live")
	}
}

func (a *app) addConfigure(menu *systray.Menu, d *Dashboard) {
	configMenu := systray.NewMenu()
	if d != nil {
		for _, provider := range []string{"codex", "claude"} {
			var roots []ClientRoot
			for _, r := range d.ClientRoots {
				if r.Provider == provider {
					roots = append(roots, r)
				}
			}
			title := "Configure " + ProviderName(provider)
			if len(roots) == 1 {
				root := roots[0]
				addAction(configMenu, title, !a.mutationBusy, func() { a.integrate(root, false) })
			} else if len(roots) > 1 {
				rootMenu := systray.NewMenu()
				for _, root := range roots {
					r := root
					addAction(rootMenu, r.Root, !a.mutationBusy, func() { a.integrate(r, false) })
				}
				configMenu.AddSubmenu(title, rootMenu)
			}
		}
	}
	addAction(configMenu, "Claude Desktop Setup…", true, func() {
		a.openDocs("https://noria.ashray.xyz/docs/native-clients/#what-about-claude-desktop")
	})
	addAction(configMenu, "Choose a Custom Client Folder…", !a.mutationBusy, func() { a.customIntegration() })
	if d != nil {
		for _, integration := range d.Integrations {
			entry := integration
			title := fmt.Sprintf("Restore Direct Settings: %s…", entry.Path)
			addAction(configMenu, title, !a.mutationBusy, func() {
				a.integrate(ClientRoot{Provider: entry.Provider, Root: filepath.Dir(entry.Path)}, true)
			})
		}
	}
	configMenu.AddSeparator()
	addAction(configMenu, "Add Codex Account…", a.loginCancel == nil, func() { a.authenticate("codex", "") })
	addAction(configMenu, "Add Claude Account…", a.loginCancel == nil, func() { a.authenticate("claude", "") })
	if a.loginCancel != nil {
		disabled(configMenu, "Finish sign-in in your browser…")
		addAction(configMenu, "Cancel Sign-In", true, func() { a.cancelLogin() })
	}
	addAction(configMenu, "Health Check", true, func() { a.healthCheck() })
	configMenu.AddSeparator()
	var p Preferences
	if d != nil {
		p = d.Preferences
	}
	addCheck(configMenu, "Launch Tray at Login", p.TrayAtLogin, !a.mutationBusy,
		func() { a.preference("tray-at-login", !p.TrayAtLogin) })
	addCheck(configMenu, "Start Noria at Login", p.StartAtLogin, !a.mutationBusy,
		func() { a.preference("start-at-login", !p.StartAtLogin) })
	refreshMenu := systray.NewMenu()
	for _, minutes := range config.QuotaRefreshIntervals {
		title := fmt.Sprintf("Every %d Minutes", minutes)
		if minutes == 0 {
			title = "On Menu Open Only (Default)"
		}
		addCheck(refreshMenu, title, p.QuotaRefreshMinutes == minutes, !a.mutationBusy,
			func() { a.mutation("preferences", "quota-refresh-minutes", fmt.Sprint(minutes), "--json") })
	}
	configMenu.AddSubmenu("Usage Auto-Refresh", refreshMenu)
	addCheck(configMenu, "Automatically Update", p.AutoUpdate, !a.mutationBusy,
		func() { a.preference("auto-update", !p.AutoUpdate) })
	updateTitle := "Check for Updates…"
	if a.updateBusy {
		updateTitle = "Checking for Updates…"
	}
	addAction(configMenu, updateTitle, !a.updateBusy, func() { a.checkUpdate(false) })
	addAction(configMenu, "Open Documentation", true, func() { a.openDocs("https://noria.ashray.xyz/docs/quickstart/") })
	configMenu.AddSeparator()
	version := a.version
	if d != nil && d.Version != "" {
		version = d.Version
	}
	disabled(configMenu, "Noria "+version)
	menu.AddSubmenu("Configure", configMenu)
}

func (a *app) integrate(root ClientRoot, remove bool) {
	go func() {
		title := "Configure " + ProviderName(root.Provider) + " for Noria?"
		if remove {
			title = "Restore direct settings?"
		}
		detail := fmt.Sprintf("%s\n\nOnly routing settings change. Your login, conversation history and other preferences stay intact. Reopen already-running clients after this change.", root.Root)
		ok, err := confirm(title, detail)
		if err != nil {
			a.errorNotice("Confirmation unavailable", err)
			return
		}
		if !ok {
			return
		}
		args := []string{"integrate", root.Provider, "--client-home", root.Root, "--yes", "--json"}
		if remove {
			args = append(args, "--remove")
		}
		a.mutation(args...)
	}()
}

func (a *app) customIntegration() {
	go func() {
		folder, ok, err := chooseFolder()
		if err != nil {
			a.errorNotice("Folder picker unavailable", err)
			return
		}
		if !ok {
			return
		}
		provider, ok, err := chooseProvider()
		if err != nil {
			a.errorNotice("Provider picker unavailable", err)
			return
		}
		if ok {
			a.integrate(ClientRoot{Provider: provider, Root: folder}, false)
		}
	}()
}

func (a *app) preference(key string, enabled bool) {
	value := "off"
	if enabled {
		value = "on"
	}
	a.mutation("preferences", key, value, "--json")
}

func (a *app) openDocs(address string) {
	if err := openURL(address); err != nil {
		a.errorNotice("Could not open documentation", err)
	}
}
