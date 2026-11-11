package app

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/amxv/noria/internal/config"
	"github.com/amxv/noria/internal/model"
)

func TestQuotaRefreshFailureIsPersistedAndVisibleInHealth(t *testing.T) {
	home := t.TempDir()
	t.Setenv("NORIA_HOME", home)
	if err := config.Init(home); err != nil {
		t.Fatal(err)
	}
	profile := filepath.Join(home, "accounts", "fixture")
	if err := config.PrivateDir(profile); err != nil {
		t.Fatal(err)
	}
	if err := config.Mutate(home, func(c *config.Config) error {
		c.Accounts = []model.Account{{ID: "fixture", Email: "example@example.com", Provider: "claude", Profile: profile, Enabled: true, Authenticated: true}}
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	const explanation = "Claude OAuth credential refresh failed; sign in again"
	if err := recordQuotaRefresh(home, []refreshResult{{Account: "fixture", Error: explanation, OK: false}}); err != nil {
		t.Fatal(err)
	}
	recorded := readQuotaRefreshStatus(home)["fixture"]
	if recorded.OK || recorded.AttemptedAt.IsZero() || recorded.Error != explanation {
		t.Fatalf("failed quota refresh not recorded: %+v", recorded)
	}
	var buf bytes.Buffer
	if err := healthCommand(home, []string{"--json"}, &buf); err != nil {
		t.Fatal(err)
	}
	var report struct {
		OK     bool `json:"ok"`
		Checks []struct {
			Name   string `json:"name"`
			OK     bool   `json:"ok"`
			Detail string `json:"detail"`
		} `json:"checks"`
	}
	if err := json.Unmarshal(buf.Bytes(), &report); err != nil {
		t.Fatal(err)
	}
	var foundFailure, stoppedHealthy bool
	for _, c := range report.Checks {
		if strings.Contains(c.Name, "claude quota") && !c.OK && strings.Contains(c.Detail, explanation) {
			foundFailure = true
		}
		if c.Name == "Local router" && c.OK && strings.Contains(c.Detail, "Stopped by choice") {
			stoppedHealthy = true
		}
	}
	if !foundFailure || !stoppedHealthy || report.OK {
		t.Fatalf("health did not expose failed quota refresh and intentional router stop: %+v", report)
	}
	dash, err := dashboard(home)
	if err != nil {
		t.Fatal(err)
	}
	accounts := dash["accounts"].([]any)
	entry := accounts[0].(map[string]any)
	last, ok := entry["last_refresh"].(quotaRefreshStatus)
	if !ok || last.Error != explanation {
		t.Fatalf("dashboard lost last refresh failure: %+v", entry)
	}
	info, err := os.Stat(filepath.Join(home, "quota-refresh.json"))
	if err != nil || info.Mode().Perm() != 0600 {
		t.Fatalf("refresh diagnostics must be private: %v %+v", err, info)
	}
	if err := recordQuotaRefresh(home, []refreshResult{{Account: "fixture", OK: true}}); err != nil {
		t.Fatal(err)
	}
	if updated := readQuotaRefreshStatus(home)["fixture"]; !updated.OK || updated.Error != "" {
		t.Fatalf("successful refresh did not clear prior error: %+v", updated)
	}
}

func TestQuotaRefreshDiagnosticsMergeAndDoNotStoreControlCharacters(t *testing.T) {
	home := t.TempDir()
	if err := config.Init(home); err != nil {
		t.Fatal(err)
	}
	if err := recordQuotaRefresh(home, []refreshResult{{Account: "a", OK: false, Error: "error\nsecret\x00"}}); err != nil {
		t.Fatal(err)
	}
	if err := recordQuotaRefresh(home, []refreshResult{{Account: "b", OK: true}}); err != nil {
		t.Fatal(err)
	}
	results := readQuotaRefreshStatus(home)
	if len(results) != 2 || results["a"].OK || !results["b"].OK || strings.ContainsAny(results["a"].Error, "\r\n\x00") {
		t.Fatalf("partial quota refresh overwrote another account or retained control characters: %+v", results)
	}
	results["a"] = quotaRefreshStatus{AttemptedAt: time.Now().Add(-time.Hour), OK: true}
	b, _ := json.Marshal(results)
	if err := config.Write(filepath.Join(home, "quota-refresh.json"), b); err != nil {
		t.Fatal(err)
	}
	if got := readQuotaRefreshStatus(home)["a"]; time.Since(got.AttemptedAt) < 30*time.Minute {
		t.Fatalf("stale success not preserved as stale: %+v", got)
	}
}
