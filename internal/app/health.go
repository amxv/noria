package app

import (
	"encoding/json"
	"fmt"
	"github.com/amxv/noria/internal/buildinfo"
	"github.com/amxv/noria/internal/config"
	"github.com/amxv/noria/internal/core"
	"github.com/amxv/noria/internal/integration"
	"github.com/amxv/noria/internal/service"
	"io"
	"net/http"
	"os"
	"os/exec"
	"runtime"
	"time"
)

// dashboard joins CLI-owned metadata for all frontends. No Swift quota logic.
func dashboard(home string) (map[string]any, error) {
	c, e := config.Load(home)
	if e != nil {
		return nil, e
	}
	b, err := controlRequest(home, http.MethodGet, "/_noria/status", nil)
	running := err == nil
	if !running {
		s, e := core.New(statePath(home), c.Accounts)
		if e != nil {
			return nil, e
		}
		b, e = json.Marshal(s.Status())
		if e != nil {
			return nil, e
		}
	}
	var v map[string]any
	if e = json.Unmarshal(b, &v); e != nil {
		return nil, e
	}
	views, e := integration.Views(home)
	if e != nil {
		return nil, e
	}
	v["schema_version"] = 1
	v["version"] = buildinfo.CurrentVersion()
	v["running"] = running
	v["address"] = c.Listen
	v["integrations"] = views
	v["client_roots"] = nativeRoots()
	v["preferences"] = map[string]bool{"start_at_login": c.StartAtLogin, "menubar_at_login": c.MenuBarAtLogin, "auto_update": c.AutoUpdate}
	if runtime.GOOS != "darwin" {
		v["preferences"].(map[string]bool)["tray_at_login"] = c.MenuBarAtLogin
	}
	v["observed_at"] = time.Now()
	refreshes := readQuotaRefreshStatus(home)
	if entries, ok := v["accounts"].([]any); ok {
		for _, entry := range entries {
			account, ok := entry.(map[string]any)
			if !ok {
				continue
			}
			id, _ := account["id"].(string)
			if result, exists := refreshes[id]; exists {
				account["last_refresh"] = result
			}
		}
	}
	return v, nil
}
func healthCommand(home string, args []string, out io.Writer) error {
	if len(args) > 0 && isHelp(args[0]) {
		fmt.Fprintln(out, "Usage: noria health [--json]\nChecks local service, configured routing and native executables. Sends no model prompt.")
		return nil
	}
	c, e := config.Load(home)
	if e != nil {
		return e
	}
	views, e := integration.Views(home)
	if e != nil {
		return e
	}
	checks := []map[string]any{}
	_, err := controlRequest(home, http.MethodGet, "/_noria/health", nil)
	running := err == nil
	wanted := service.ReadIntent(home).Wanted
	routerDetail := c.Listen
	if !running && !wanted {
		routerDetail = "Stopped by choice (quota refresh works while stopped)"
	}
	if !running && wanted {
		routerDetail = "Expected to be running, but is unreachable at " + c.Listen
	}
	checks = append(checks, map[string]any{"name": "Local router", "ok": running || !wanted, "detail": routerDetail})
	refreshes := readQuotaRefreshStatus(home)
	for _, a := range c.Accounts {
		if !a.Enabled {
			continue
		}
		name := a.DisplayName()
		if name == "" {
			name = a.Provider
		}
		check := map[string]any{"name": a.Provider + " quota · " + name, "ok": false}
		if last, found := refreshes[a.ID]; found {
			if !last.OK {
				check["detail"] = "Last refresh failed: " + last.Error
			}
			if last.OK && time.Since(last.AttemptedAt) <= 3*time.Minute {
				check["ok"] = true
				check["detail"] = "Usage refreshed " + last.AttemptedAt.Local().Format("3:04 PM")
			}
			if last.OK && check["ok"] == false {
				check["detail"] = "Quota snapshot is stale; refresh usage to get current values"
			}
		} else {
			check["detail"] = "No quota refresh recorded; use Refresh Usage"
		}
		checks = append(checks, check)
	}
	for _, view := range views {
		checks = append(checks, map[string]any{"name": view.Provider + " settings", "ok": view.Healthy, "detail": view.Path})
	}
	seen := map[string]bool{}
	for _, a := range c.Accounts {
		if seen[a.Provider] || !a.Enabled {
			continue
		}
		seen[a.Provider] = true
		_, e := exec.LookPath(a.Provider)
		checks = append(checks, map[string]any{"name": a.Provider + " executable", "ok": e == nil})
	}
	for _, key := range []string{"ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY"} {
		if os.Getenv(key) != "" {
			checks = append(checks, map[string]any{"name": "Environment override: " + key, "ok": false, "detail": "May bypass settings.json or override the gateway helper. Clear it for persistent routing."})
		}
	}
	ok := true
	for _, v := range checks {
		ok = ok && v["ok"] == true
	}
	report := map[string]any{"ok": ok, "checks": checks, "note": "Quota health reflects the latest recorded read; run Refresh Usage to check the provider again. No model request was sent."}
	if len(args) == 1 && args[0] == "--json" {
		return printJSON(out, report)
	}
	for _, v := range checks {
		fmt.Fprintf(out, "%s: %t\n", v["name"], v["ok"])
	}
	return nil
}
