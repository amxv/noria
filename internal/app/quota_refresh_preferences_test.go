package app

import (
	"bytes"
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	"github.com/amxv/noria/internal/config"
)

func TestQuotaRefreshPreferenceDefaultAndRoundTrip(t *testing.T) {
	home := t.TempDir()
	t.Setenv("NORIA_HOME", home)
	var out, errOut bytes.Buffer
	run := func(args ...string) map[string]any {
		t.Helper()
		out.Reset()
		errOut.Reset()
		if err := Run(args, &out, &errOut); err != nil {
			t.Fatalf("%v failed: %v (%s)", args, err, errOut.String())
		}
		var p map[string]any
		if err := json.Unmarshal(out.Bytes(), &p); err != nil {
			t.Fatalf("%v returned invalid JSON: %s (%v)", args, out.String(), err)
		}
		return p
	}
	initial := run("preferences", "--json")
	if initial["quota_refresh_minutes"] != float64(0) {
		t.Fatalf("new Noria home must refresh only when menu opens: %v", initial)
	}
	for _, minutes := range config.QuotaRefreshIntervals {
		p := run("preferences", "quota-refresh-minutes", fmt.Sprint(minutes), "--json")
		if p["quota_refresh_minutes"] != float64(minutes) {
			t.Fatalf("setting %d failed: %v", minutes, p)
		}
		c, err := config.Load(home)
		if err != nil || c.QuotaRefreshMinutes != minutes {
			t.Fatalf("setting %d not persisted: %+v %v", minutes, c, err)
		}
		state, err := dashboard(home)
		if err != nil {
			t.Fatal(err)
		}
		prefs := state["preferences"].(map[string]any)
		if prefs["quota_refresh_minutes"] != minutes {
			t.Fatalf("dashboard must expose %d-minute interval: %+v", minutes, prefs)
		}
	}
	p := run("preferences", "quota-refresh-minutes", "on-open", "--json")
	if p["quota_refresh_minutes"] != float64(0) {
		t.Fatalf("on-open alias did not disable automatic refresh: %+v", p)
	}
	// Existing CLI preference flags and auto-update defaults remain untouched.
	if p["auto_update"] != true || p["start_at_login"] != false {
		t.Fatalf("quota refresh setting mutated other preferences: %+v", p)
	}
	p = run("preferences", "--json")
	if p["quota_refresh_minutes"] != float64(0) {
		t.Fatalf("reading preference after setting lost default: %+v", p)
	}
	for _, invalid := range []string{"1", "3", "4", "20", "61", "181", "-1", "abc", "off", "2.5"} {
		out.Reset()
		if err := Run([]string{"preferences", "quota-refresh-minutes", invalid, "--json"}, &out, &errOut); err == nil {
			t.Fatalf("unsupported %q interval was accepted", invalid)
		}
		c, err := config.Load(home)
		if err != nil || c.QuotaRefreshMinutes != 0 {
			t.Fatalf("rejected value %q modified saved preference: %+v %v", invalid, c, err)
		}
	}
	if err := Run([]string{"preferences", "quota-refresh-minutes", "--json"}, &out, &errOut); err == nil {
		t.Fatal("missing refresh interval accepted")
	}
	out.Reset()
	if err := Run([]string{"preferences", "--help"}, &out, &errOut); err != nil || !strings.Contains(out.String(), "quota-refresh-minutes") {
		t.Fatalf("CLI help omitted refresh interval: %s %v", out.String(), err)
	}
}
