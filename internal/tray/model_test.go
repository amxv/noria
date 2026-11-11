package tray

import (
	"math"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestDashboardFixture(t *testing.T) {
	fixture := filepath.Join("..", "..", "apps", "menubar", "Tests", "status.json")
	b, err := os.ReadFile(fixture)
	if err != nil {
		t.Fatal(err)
	}
	d, err := ParseDashboard(b)
	if err != nil {
		t.Fatal(err)
	}
	if !d.Running || len(d.Accounts) != 1 || d.Accounts[0].Title() != "alex@example.com" {
		t.Fatalf("unexpected dashboard: %+v", d)
	}
	if d.Preferences.QuotaRefreshMinutes != 0 {
		t.Fatal("older dashboards must default to on-open")
	}
	rows := d.Accounts[0].VisibleWindows()
	if len(rows) != 2 || !strings.Contains(rows[0].Text(time.Date(2026, 10, 22, 0, 0, 0, 0, time.UTC)), "75% left") {
		t.Fatalf("unexpected quota rows: %+v", rows)
	}
}
func TestQuotaFiltering(t *testing.T) {
	used := 31.0
	nan := math.NaN()
	a := Account{Snapshot: &Snapshot{Windows: []QuotaWindow{
		{Duration: int64(5 * time.Hour), UsedPercent: nil},
		{Duration: int64(5 * time.Hour), UsedPercent: &nan},
		{Duration: int64(5 * time.Hour), UsedPercent: &used, Model: "special"},
		{Duration: int64(7 * 24 * time.Hour), UsedPercent: &used},
	}}}
	rows := a.VisibleWindows()
	if len(rows) != 1 || rows[0].Title != "Week" {
		t.Fatalf("expected only reported general weekly quota, got %+v", rows)
	}
}
func TestFableQuotaReportedPerClaudeAccountOnly(t *testing.T) {
	fixture := filepath.Join("..", "..", "apps", "menubar", "Tests", "fable-status.json")
	b, err := os.ReadFile(fixture)
	if err != nil {
		t.Fatal(err)
	}
	d, err := ParseDashboard(b)
	if err != nil {
		t.Fatal(err)
	}
	if len(d.Accounts) != 3 {
		t.Fatalf("invalid fixture: %+v", d)
	}
	if d.Preferences.QuotaRefreshMinutes != 15 {
		t.Fatal("nonzero persisted refresh interval was not decoded")
	}
	eligible, missing, codex := d.Accounts[0].VisibleWindows(), d.Accounts[1].VisibleWindows(), d.Accounts[2].VisibleWindows()
	if len(eligible) != 3 || eligible[2].Title != "Fable Week" ||
		!strings.Contains(eligible[2].Text(time.Date(2026, 11, 11, 0, 0, 0, 0, time.UTC)), "100% left") ||
		len(missing) != 1 || missing[0].Title != "Week" || len(codex) != 1 || codex[0].Title != "Week" {
		t.Fatalf("optional Claude Fable row incorrect: eligible=%+v missing=%+v codex=%+v", eligible, missing, codex)
	}
}

func TestUsageAutoRefreshTimerOnlyWhenConfiguredAndDue(t *testing.T) {
	now := time.Now()
	for _, minutes := range []int{0, -1} {
		if usageRefreshDue(minutes, now.Add(-4*time.Hour), now) {
			t.Fatalf("%d-minute setting started an automatic refresh", minutes)
		}
	}
	if usageRefreshDue(5, time.Time{}, now) {
		t.Fatal("missing last refresh initiated timer")
	}
	if usageRefreshDue(5, now.Add(-4*time.Minute), now) {
		t.Fatal("usage refresh occurred before the interval")
	}
	if !usageRefreshDue(5, now.Add(-5*time.Minute), now) {
		t.Fatal("five-minute refresh did not become due")
	}
	if usageRefreshDue(180, now.Add(-179*time.Minute), now) {
		t.Fatal("180-minute refresh occurred early")
	}
	if !usageRefreshDue(180, now.Add(-180*time.Minute), now) {
		t.Fatal("180-minute refresh was not due")
	}
}
func TestTrayHidesStaleAndFailedRefreshPercentages(t *testing.T) {
	used := 23.0
	now := time.Now()
	a := Account{Provider: "claude", Snapshot: &Snapshot{Available: true, ObservedAt: now, Windows: []QuotaWindow{{Name: "seven_day_fable", Model: "fable", Duration: int64(7 * 24 * time.Hour), UsedPercent: &used}}}}
	if rows := a.CurrentWindows(now); len(rows) != 1 {
		t.Fatalf("fresh server quota missing: %+v", rows)
	}
	a.LastRefresh = &QuotaRefreshStatus{AttemptedAt: now.Add(time.Second), Error: "auth failure"}
	if a.QuotaIsCurrent(now) || len(a.CurrentWindows(now)) != 0 {
		t.Fatal("failed refresh still renders old quota as live")
	}
	a.LastRefresh = nil
	a.Snapshot.ObservedAt = now.Add(-10 * time.Minute)
	if a.QuotaIsCurrent(now) || len(a.CurrentWindows(now)) != 0 {
		t.Fatal("stale quota still renders as current")
	}
}
func TestInvalidDashboardSchema(t *testing.T) {
	if _, err := ParseDashboard([]byte(`{"schema_version":2}`)); err == nil {
		t.Fatal("future schema accepted")
	}
}
