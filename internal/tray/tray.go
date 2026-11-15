//go:build windows || linux

package tray

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/amxv/noria/internal/config"
	"github.com/gogpu/systray"
)

type app struct {
	cli               CLI
	tray              *systray.SystemTray
	version           string
	mu                sync.Mutex
	dashboard         *Dashboard
	statusBusy        bool
	statusPending     bool
	quotaPending      bool
	refreshing        map[string]bool
	refreshFailures   []string
	mutationBusy      bool
	loginCancel       context.CancelFunc
	updateBusy        bool
	refreshMessage    string
	lastQuotaPoll     time.Time
	lastQuotaInterval int
	closed            chan struct{}
	stopOnce          sync.Once
}

func Run(home, version string) error {
	if err := prepareDesktop(); err != nil {
		return err
	}
	cli, err := newCLI(home)
	if err != nil {
		return err
	}
	if err := config.Init(cli.Home); err != nil {
		return err
	}
	// The same OS-level advisory lock works for both GUI launches and CLI
	// relaunch coordination, without stale PID files after a crash.
	ctx, cancel := context.WithTimeout(context.Background(), 250*time.Millisecond)
	defer cancel()
	unlock, err := config.LockContext(ctx, cli.Home, "tray")
	if err != nil {
		return nil
	} // second launch leaves the first tray intact
	defer unlock()
	// A quit request written while no tray was running must not unexpectedly
	// terminate the next GUI launch.
	_ = os.Remove(filepath.Join(cli.Home, "tray-control.json"))
	a := &app{cli: cli, version: version, closed: make(chan struct{}), lastQuotaInterval: -1}
	a.tray = systray.New()
	a.tray.SetAppName("Noria").SetIcon(iconPNG(false)).SetTooltip("Noria · loading").SetMenu(a.buildMenu()).Show()
	a.tray.OnClick(func() { a.loadStatus(); a.refreshQuota() })
	a.tray.OnRightClick(func() { a.loadStatus(); a.refreshQuota() })
	go a.background()
	defer a.quit()
	return a.tray.Run()
}

func (a *app) quit() {
	a.stopOnce.Do(func() { close(a.closed); a.tray.Remove() })
}

func (a *app) background() {
	a.loadStatus()
	a.syncUpdateAgent()
	// Keep the menu useful even when Linux's AppIndicator host doesn't emit an
	// open-menu event. No automatic provider reads occur by default.
	statusTick := time.NewTicker(30 * time.Second)
	quotaTick := time.NewTicker(15 * time.Second)
	updateTick := time.NewTicker(time.Hour)
	controlTick := time.NewTicker(time.Second)
	defer statusTick.Stop()
	defer quotaTick.Stop()
	defer updateTick.Stop()
	defer controlTick.Stop()
	check := time.NewTimer(30 * time.Second)
	defer check.Stop()
	for {
		select {
		case <-a.closed:
			return
		case <-statusTick.C:
			a.loadStatus()
		case <-quotaTick.C:
			a.autoRefreshIfDue()
		case <-updateTick.C:
			a.checkUpdate(true)
		case <-check.C:
			a.checkUpdate(true)
		case <-controlTick.C:
			a.checkControl()
		}
	}
}

func (a *app) autoRefreshIfDue() {
	a.mu.Lock()
	minutes := 0
	if a.dashboard != nil {
		minutes = a.dashboard.Preferences.QuotaRefreshMinutes
	}
	now := time.Now()
	if minutes != a.lastQuotaInterval {
		a.lastQuotaInterval = minutes
		a.lastQuotaPoll = now // Arm from the saved preference, not from launch.
	}
	due := !a.quotaBusy() && usageRefreshDue(minutes, a.lastQuotaPoll, now)
	a.mu.Unlock()
	if due {
		a.refreshQuota()
	}
}

func (a *app) checkControl() {
	file := filepath.Join(a.cli.Home, "tray-control.json")
	b, err := os.ReadFile(file)
	if err != nil {
		return
	}
	var command struct {
		Action      string    `json:"action"`
		RequestedAt time.Time `json:"requested_at"`
	}
	if json.Unmarshal(b, &command) != nil {
		return
	}
	_ = os.Remove(file)
	if command.Action == "quit" && time.Since(command.RequestedAt) < 2*time.Minute && time.Until(command.RequestedAt) < time.Minute {
		a.quit()
	}
}

func (a *app) renderLocked() {
	select {
	case <-a.closed:
		return
	default:
	}
	a.tray.SetMenu(a.buildMenu())
	running := a.dashboard != nil && a.dashboard.Running
	a.tray.SetIcon(iconPNG(running))
	status := "stopped"
	if a.dashboard == nil {
		status = "loading"
	} else if running {
		status = "running"
	}
	a.tray.SetTooltip("Noria · " + status)
}

func (a *app) loadStatus() {
	a.mu.Lock()
	if a.statusBusy {
		// Another account finished mid-load; reload once more afterwards so
		// its fresh numbers are not dropped.
		a.statusPending = true
		a.mu.Unlock()
		return
	}
	a.statusBusy = true
	a.mu.Unlock()
	go func() {
		data, err := a.cli.run(8*time.Second, "status", "--json")
		var dashboard Dashboard
		if err == nil {
			dashboard, err = ParseDashboard(data)
		}
		a.mu.Lock()
		a.statusBusy = false
		pending := a.statusPending
		a.statusPending = false
		if err == nil {
			a.dashboard = &dashboard
		}
		quota := a.quotaPending && a.dashboard != nil
		if quota {
			a.quotaPending = false
		}
		a.renderLocked()
		a.mu.Unlock()
		if pending {
			a.loadStatus()
		}
		if quota {
			a.refreshQuota()
		}
		if err == nil && dashboard.Version != a.version && a.version != "dev" && dashboard.Version != "dev" {
			// Reopen against the newly installed binary, including automatic upgrades.
			if _, err := a.cli.run(5*time.Second, "tray", "reopen"); err == nil {
				a.quit()
			}
		}
	}()
}

func (a *app) mutation(args ...string) {
	a.mu.Lock()
	if a.mutationBusy {
		a.mu.Unlock()
		return
	}
	a.mutationBusy = true
	a.renderLocked()
	a.mu.Unlock()
	go func() {
		_, err := a.cli.run(55*time.Second, args...)
		a.mu.Lock()
		a.mutationBusy = false
		a.renderLocked()
		a.mu.Unlock()
		if err != nil {
			a.errorNotice("Noria could not apply the change", err)
		}
		a.loadStatus()
	}()
}

// refreshQuota reads every account independently so a slow provider never
// holds back another account's numbers; each finished read reloads status.
func (a *app) refreshQuota() {
	a.mu.Lock()
	if a.dashboard == nil {
		// Refresh as soon as the first status load reports the accounts.
		a.quotaPending = true
		a.mu.Unlock()
		a.loadStatus()
		return
	}
	if a.refreshing == nil {
		a.refreshing = map[string]bool{}
	}
	if len(a.refreshing) == 0 {
		a.refreshFailures, a.refreshMessage = nil, ""
	}
	targets := []string{}
	for _, account := range a.dashboard.Accounts {
		if (account.Enabled || account.Email != "") && !a.refreshing[account.ID] {
			a.refreshing[account.ID] = true
			targets = append(targets, account.ID)
		}
	}
	if len(targets) > 0 {
		a.lastQuotaPoll = time.Now()
	}
	a.renderLocked()
	a.mu.Unlock()
	for _, id := range targets {
		go func(id string) {
			_, err := a.cli.run(60*time.Second, "quota", "refresh", id)
			a.mu.Lock()
			delete(a.refreshing, id)
			if err != nil {
				a.refreshFailures = append(a.refreshFailures, id)
			}
			if len(a.refreshing) == 0 && len(a.refreshFailures) > 0 {
				a.refreshMessage = "Refresh failed · check account details"
			}
			a.renderLocked()
			a.mu.Unlock()
			a.loadStatus()
		}(id)
	}
}

func (a *app) quotaBusy() bool { return len(a.refreshing) > 0 }

func (a *app) authenticate(provider, id string) {
	a.mu.Lock()
	if a.loginCancel != nil {
		a.mu.Unlock()
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 310*time.Second)
	a.loginCancel = cancel
	a.renderLocked()
	a.mu.Unlock()
	args := []string{"account", "add", provider, "--json"}
	if id != "" {
		args = []string{"account", "relogin", id, "--json"}
	}
	go func() {
		_, err := a.cli.runContext(ctx, args...)
		wasCanceled := errors.Is(ctx.Err(), context.Canceled)
		a.mu.Lock()
		a.loginCancel = nil
		a.renderLocked()
		a.mu.Unlock()
		cancel()
		if err != nil && !wasCanceled {
			a.errorNotice("Sign-in did not complete", err)
		}
		a.loadStatus()
		a.refreshQuota()
	}()
}

func (a *app) cancelLogin() {
	a.mu.Lock()
	cancel := a.loginCancel
	a.mu.Unlock()
	if cancel != nil {
		cancel()
	}
}

func (a *app) syncUpdateAgent() {
	go func() { _, _ = a.cli.run(20*time.Second, "__sync-updates") }()
}

func (a *app) checkUpdate(automatic bool) {
	a.mu.Lock()
	if a.updateBusy || a.mutationBusy || a.loginCancel != nil {
		a.mu.Unlock()
		return
	}
	a.updateBusy = true
	a.renderLocked()
	a.mu.Unlock()
	go func() {
		args := []string{"upgrade", "--check", "--json"}
		if automatic {
			args = []string{"upgrade", "--automatic", "--json"}
		}
		b, err := a.cli.run(185*time.Second, args...)
		a.mu.Lock()
		a.updateBusy = false
		a.renderLocked()
		a.mu.Unlock()
		if err != nil {
			if !automatic {
				a.errorNotice("Update check failed", err)
			}
			return
		}
		if len(b) == 0 {
			return
		}
		var result UpdateResult
		if json.Unmarshal(b, &result) != nil {
			return
		}
		if result.Updated {
			a.loadStatus()
			return
		}
		if !automatic {
			if !result.Available {
				message("Noria is up to date", "Version "+result.Current)
				return
			}
			ok, err := confirm("Noria "+result.Latest+" is available", "The CLI verifies checksums and waits for active requests to finish before upgrading.")
			if err != nil {
				a.errorNotice("Confirmation unavailable", err)
			} else if ok {
				a.applyUpdate()
			}
		}
	}()
}
func (a *app) applyUpdate() {
	a.mu.Lock()
	if a.updateBusy {
		a.mu.Unlock()
		return
	}
	a.updateBusy = true
	a.renderLocked()
	a.mu.Unlock()
	go func() {
		b, err := a.cli.run(185*time.Second, "upgrade", "--json")
		a.mu.Lock()
		a.updateBusy = false
		a.renderLocked()
		a.mu.Unlock()
		if err != nil {
			a.errorNotice("Noria update failed", err)
			return
		}
		var result UpdateResult
		if json.Unmarshal(b, &result) == nil {
			if result.Updated {
				a.loadStatus()
				return
			}
			if result.Message != "" {
				a.tray.ShowNotification("Noria Update", result.Message)
			}
		}
	}()
}

func (a *app) healthCheck() {
	go func() {
		b, err := a.cli.run(20*time.Second, "health", "--json")
		if err != nil {
			a.errorNotice("Health Check", err)
			return
		}
		var report HealthReport
		if json.Unmarshal(b, &report) != nil {
			return
		}
		lines := []string{}
		for _, check := range report.Checks {
			prefix := "✓"
			if !check.OK {
				prefix = "•"
			}
			lines = append(lines, fmt.Sprintf("%s %s\n%s", prefix, check.Name, check.Detail))
		}
		message("Noria Health Check", strings.Join(lines, "\n\n")+"\n\nNo model request was sent.")
	}()
}
func (a *app) errorNotice(title string, err error) {
	detail := err.Error()
	if len(detail) > 245 {
		detail = detail[:245]
	}
	a.tray.ShowNotification(title, detail)
}
