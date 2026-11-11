package provider

import (
	"bufio"
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/amxv/noria/internal/config"
	"github.com/amxv/noria/internal/model"
)

func TestMain(m *testing.M) {
	if os.Getenv("GO_PROVIDER_FIXTURE") == "native-login" {
		nativeLoginFixture()
		os.Exit(0)
	}
	if os.Getenv("GO_PROVIDER_FIXTURE") == "codex" {
		codexFixture()
		os.Exit(0)
	}
	if os.Getenv("GO_PROVIDER_FIXTURE") == "claude" {
		claudeFixture()
		os.Exit(0)
	}
	os.Exit(m.Run())
}

func codexFixture() {
	p := os.Getenv("CODEX_HOME")
	if p == "" || os.Getenv("OPENAI_API_KEY") != "" || os.Getenv("ANTHROPIC_BASE_URL") != "" {
		os.Exit(3)
	}
	if os.Getenv("GO_FIXTURE_HANG") == "1" {
		ticker := time.NewTicker(time.Hour)
		for range ticker.C {
		}
	}
	f, err := os.OpenFile(filepath.Join(p, "calls"), os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0600)
	if err != nil {
		os.Exit(4)
	}
	defer f.Close()
	scan := bufio.NewScanner(os.Stdin)
	enc := json.NewEncoder(os.Stdout)
	for scan.Scan() {
		var req struct {
			ID     int             `json:"id"`
			Method string          `json:"method"`
			Params json.RawMessage `json:"params"`
		}
		if json.Unmarshal(scan.Bytes(), &req) != nil {
			os.Exit(5)
		}
		_, _ = fmt.Fprintln(f, req.Method)
		var result any = map[string]any{}
		switch req.Method {
		case "initialize":
			if !strings.Contains(string(req.Params), `"experimentalApi":true`) {
				os.Exit(6)
			}
		case "initialized":
			continue
		case "account/read":
			var params struct {
				Refresh bool `json:"refreshToken"`
			}
			if json.Unmarshal(req.Params, &params) != nil {
				os.Exit(7)
			}
			_, _ = fmt.Fprintf(f, "refreshToken=%t\n", params.Refresh)
			if params.Refresh {
				tmp := filepath.Join(p, "auth.tmp")
				_ = os.WriteFile(tmp, []byte(`{"tokens":{"access_token":"fresh-fixture-token","account_id":"fixture-account"}}`), 0600)
				_ = os.Rename(tmp, filepath.Join(p, "auth.json"))
			}
			result = map[string]any{"account": map[string]string{"type": "chatgpt"}, "requiresOpenaiAuth": true}
		case "account/rateLimits/read":
			if string(req.Params) != "{}" {
				os.Exit(8)
			}
			result = map[string]any{"rateLimits": map[string]any{"primary": map[string]any{"usedPercent": 12.5, "windowDurationMins": 300}}}
		default:
			os.Exit(9)
		}
		_ = enc.Encode(map[string]any{"id": req.ID, "result": result})
	}
}

func fixtureProfile(t *testing.T, provider string) (*Manager, model.Account) {
	t.Helper()
	home := t.TempDir()
	p := filepath.Join(home, "profiles", "fixture")
	if err := os.MkdirAll(p, 0700); err != nil {
		t.Fatal(err)
	}
	return New(home), model.Account{ID: "fixture", Provider: provider, Profile: p, Enabled: true}
}

func TestCodexReversedNullAndScoped(t *testing.T) {
	s, err := parseCodex("a", []byte(`{"rateLimits":{"primary":{"usedPercent":99}},"rateLimitsByLimitId":{"chatgpt":{"primary":{"usedPercent":0}},"codex":{"primary":{"usedPercent":75,"windowDurationMins":10080,"resetsAt":2000000000},"secondary":{"usedPercent":null,"windowDurationMins":300,"resetsAt":null}},"codex_model":{"limitName":"GPT-5.3-Codex","primary":{"usedPercent":100,"windowDurationMins":10080},"ordinaryUsageAllowed":true},"codex_ignored":{"primary":{"usedPercent":100}},"other":{"limitName":"other","primary":{"usedPercent":100}}}}`), time.Now())
	if err != nil {
		t.Fatal(err)
	}
	if len(s.Windows) != 4 || s.Windows[0].Name != "seven_day" || *s.Windows[0].Used != 75 || s.Windows[0].ResetsAt.Unix() != 2000000000 {
		t.Fatalf("wrong canonical/reversed windows: %+v", s)
	}
	if s.Windows[1].Used != nil || s.Windows[1].ResetsAt != nil || s.Windows[1].Name != "five_hour" {
		t.Fatal("null metadata was synthesized")
	}
	if s.Windows[2].Model != "GPT-5.3-Codex" || s.Windows[2].Allowed == nil || !*s.Windows[2].Allowed || s.Windows[2].Exhausted {
		t.Fatal("scoped authoritative allowance lost")
	}
}

func TestCodexUnknownMalformedAndBackendBlock(t *testing.T) {
	for _, raw := range []string{`{"rateLimitsByLimitId":{"chatgpt":{}}}`, `{"rateLimits":{"primary":{"usedPercent":101}}}`, `{"rateLimits":{"primary":{"resetsAt":"bad"}}}`} {
		if _, err := parseCodex("a", []byte(raw), time.Now()); err == nil {
			t.Fatal("accepted malformed or unrelated quota")
		}
	}
	s, err := parseCodex("a", []byte(`{"rateLimits":{"primary":null,"secondary":null,"ordinaryUsageAllowed":false}}`), time.Now())
	if err != nil || len(s.Windows) != 1 || !s.Windows[0].Exhausted {
		t.Fatal("backend block lost")
	}
	s, err = parseCodex("a", []byte(`{"rateLimits":{"primary":{"usedPercent":0},"secondary":{"usedPercent":null}}}`), time.Now())
	if err != nil || s.Windows[0].Used == nil || *s.Windows[0].Used != 0 || s.Windows[1].Used != nil {
		t.Fatal("zero and unknown conflated")
	}
}

func TestClaudePartialScopedAndISOOffset(t *testing.T) {
	s, err := parseClaude("a", []byte(`{"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":12.5,"resets_at":"2026-10-20T01:02:03+05:30"},"seven_day":null,"seven_day_sonnet":{"utilization":null,"resets_at":null}}}`), time.Now())
	if err != nil {
		t.Fatal(err)
	}
	if !s.Available || len(s.Windows) != 2 || *s.Windows[0].Used != 12.5 || s.Windows[1].Model != "sonnet" || s.Windows[1].Used != nil {
		t.Fatalf("bad partial usage: %+v", s)
	}
	want, _ := time.Parse(time.RFC3339, "2026-10-19T19:32:03Z")
	if !s.Windows[0].ResetsAt.Equal(want) {
		t.Fatal("ISO offset ignored")
	}
	for _, raw := range []string{`{"rate_limits_available":false,"rate_limits":null}`, `{"rate_limits_available":true,"rate_limits":null}`} {
		s, err = parseClaude("a", []byte(raw), time.Now())
		if err != nil || s.Available || s.AuthInvalid || len(s.Windows) != 0 {
			t.Fatal("unavailable telemetry became usable/auth invalid")
		}
	}
	for _, raw := range []string{`{}`, `{"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":101,"resets_at":null}}}`, `{"rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":2,"resets_at":"2026-10-20T00:00:00"}}}`} {
		if _, err := parseClaude("a", []byte(raw), time.Now()); err == nil {
			t.Fatal("accepted malformed Claude schema")
		}
	}
}

func TestClaudeFableWeeklyWindowFromOAuthLimits(t *testing.T) {
	// Fable is in limits[] rather than the older seven_day_fable key. The
	// endpoint may include billing metadata and unrelated model quotas.
	raw := `{"rate_limits_available":true,"rate_limits":{
"five_hour":{"utilization":3,"resets_at":null},
"seven_day":{"utilization":0,"resets_at":"2026-11-15T15:59:59Z"},
"limits":[
 {"kind":"session","group":"session","percent":3,"scope":null,"resets_at":null},
 {"kind":"weekly_scoped","group":"weekly","percent":15,"scope":{"model":{"display_name":"Sonnet"}},"resets_at":null},
 {"kind":"weekly_scoped","group":"weekly","percent":0,"is_active":false,"scope":{"model":{"id":null,"display_name":"Fable"}},"resets_at":"2026-11-15T15:59:59.519837+00:00"}
],"extra_usage":{"is_enabled":false},"weekly_scoped_shares":{"unknown":"ignored"}}}`
	s, err := parseClaude("test", []byte(raw), time.Now())
	if err != nil || len(s.Windows) != 3 || !hasClaudeFableWindow(s) {
		t.Fatalf("optional Fable window was lost: %+v %v", s, err)
	}
	w := s.Windows[2]
	if w.Model != "fable" || w.Name != "seven_day_fable" || w.LimitID != "seven_day_fable" || w.Duration != 7*24*time.Hour || w.Used == nil || *w.Used != 0 || w.ResetsAt == nil || !w.ResetsAt.Equal(time.Date(2026, 11, 15, 15, 59, 59, 519837000, time.UTC)) {
		t.Fatalf("incorrect Fable usage or reset: %+v", w)
	}
}

func TestClaudeLiveCountersSupersedeLegacyCachedCounters(t *testing.T) {
	// Mirrors the observed discrepancy: legacy 7/6/11 used vs live 0/12/23.
	raw := `{"rate_limits_available":true,"rate_limits":{
"five_hour":{"utilization":7,"resets_at":"2026-11-11T11:29:59Z"},
"seven_day":{"utilization":6,"resets_at":"2026-11-15T15:59:59Z"},
"seven_day_fable":{"utilization":11,"resets_at":"2026-11-15T15:59:59Z"},
"limits":[
{"kind":"session","group":"session","percent":0,"resets_at":"2026-11-11T21:59:59Z"},
{"kind":"weekly_all","group":"weekly","percent":12,"resets_at":"2026-11-15T15:59:59Z"},
{"kind":"weekly_scoped","group":"weekly","percent":23,"resets_at":"2026-11-15T15:59:59Z","scope":{"model":{"display_name":"Fable"}}}]
}}`
	s, err := parseClaude("a", []byte(raw), time.Now())
	if err != nil || len(s.Windows) != 3 {
		t.Fatalf("cannot parse current OAuth counters: %+v %v", s, err)
	}
	for _, want := range []struct {
		name  string
		value float64
	}{{"five_hour", 0}, {"seven_day", 12}, {"seven_day_fable", 23}} {
		found := false
		for _, w := range s.Windows {
			if w.Name == want.name && w.Used != nil && *w.Used == want.value {
				found = true
			}
		}
		if !found {
			t.Fatalf("expected live %s=%.0f, got %+v", want.name, want.value, s.Windows)
		}
	}
}

func TestClaudeFableOnlyWhenServerReportsIt(t *testing.T) {
	for _, raw := range []string{
		`{"rate_limits_available":true,"rate_limits":{"seven_day":{"utilization":22,"resets_at":null}}}`,
		`{"rate_limits_available":true,"rate_limits":{"seven_day":{"utilization":22,"resets_at":null},"limits":[{"kind":"weekly_scoped","group":"weekly","percent":0,"scope":{"model":{"display_name":"Sonnet"}}}]}}`,
		`{"rate_limits_available":true,"rate_limits":{"seven_day":{"utilization":22,"resets_at":null},"limits":[]}}`,
	} {
		s, err := parseClaude("test", []byte(raw), time.Now())
		if err != nil || len(s.Windows) != 1 || hasClaudeFableWindow(s) {
			t.Fatalf("Fable quota was inferred without a server-reported bucket: %+v %v", s, err)
		}
	}
}

func TestClaudeFableNativeModelScopedAndDuplicatePrecedence(t *testing.T) {
	raw := `{"rate_limits_available":true,"rate_limits":{
"seven_day_fable":{"utilization":51,"resets_at":null},
"model_scoped":[{"display_name":"Fable 5.2","utilization":24,"resets_at":"2026-11-15T15:59:59Z"},{"display_name":"Opus","utilization":30,"resets_at":null}]}}`
	s, err := parseClaude("test", []byte(raw), time.Now())
	if err != nil || len(s.Windows) != 1 || !hasClaudeFableWindow(s) || s.Windows[0].Used == nil || *s.Windows[0].Used != 24 {
		t.Fatalf("native Fable bucket was not preferred over legacy: %+v %v", s, err)
	}
}

func TestClaudeFableMalformedQuotaFailsClosed(t *testing.T) {
	for _, fields := range []string{
		`"percent":101,"resets_at":null`,
		`"percent":-1,"resets_at":null`,
		`"percent":"0","resets_at":null`,
		`"percent":0,"resets_at":"2026-11-15T15:59:59"`,
		`"resets_at":null`,
	} {
		raw := `{"rate_limits_available":true,"rate_limits":{"limits":[{"kind":"weekly_scoped","group":"weekly","scope":{"model":{"display_name":"Fable"}},` + fields + `}]}}`
		if _, err := parseClaude("test", []byte(raw), time.Now()); err == nil {
			t.Fatalf("accepted malformed Fable quota: %s", fields)
		}
	}
}

func TestCodexNativeRefreshAndCredentialIsolation(t *testing.T) {
	m, a := fixtureProfile(t, "codex")
	t.Setenv("GO_PROVIDER_FIXTURE", "codex")
	t.Setenv("OPENAI_API_KEY", "must-not-leak")
	t.Setenv("ANTHROPIC_BASE_URL", "http://127.0.0.1:43119")
	m.codexCommand = os.Args[0]
	expired := "e." + base64.RawURLEncoding.EncodeToString([]byte(`{"exp":1}`)) + ".s"
	data, _ := json.Marshal(map[string]any{"tokens": map[string]string{"access_token": expired}})
	if err := os.WriteFile(filepath.Join(a.Profile, "auth.json"), data, 0600); err != nil {
		t.Fatal(err)
	}
	var wg sync.WaitGroup
	m2 := New(m.home)
	m2.codexCommand = os.Args[0]
	for i := range 20 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			manager := m
			if i%2 == 0 {
				manager = m2
			}
			c, err := manager.Credentials(context.Background(), a)
			if err != nil || c.Token != "fresh-fixture-token" || c.AccountID != "fixture-account" {
				t.Errorf("credential refresh failed: %v", err)
			}
		}()
	}
	wg.Wait()
	calls, err := os.ReadFile(filepath.Join(a.Profile, "calls"))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Count(string(calls), "account/read") != 1 {
		t.Fatalf("expected one rotating native refresh; calls=%s", calls)
	}
	s, err := m.Refresh(context.Background(), a)
	if err != nil || !s.Available || len(s.Windows) != 1 {
		t.Fatalf("control quota failed: %v", err)
	}
	calls, err = os.ReadFile(filepath.Join(a.Profile, "calls"))
	if err != nil || strings.Count(string(calls), "refreshToken=true") != 1 || strings.Count(string(calls), "refreshToken=false") != 1 {
		t.Fatalf("passive quota reads must not rotate credentials: %s (%v)", calls, err)
	}
	relock, err := config.Lock(a.Profile, "native")
	if err != nil {
		t.Fatal("lock ownership leaked", err)
	}
	relock()
	outside := t.TempDir()
	a.Profile = outside
	if _, err := m.Credentials(context.Background(), a); err == nil {
		t.Fatal("accepted external profile")
	}
}

func TestNativeDeadlineAndCrossProcessLock(t *testing.T) {
	m, a := fixtureProfile(t, "codex")
	m.codexCommand = os.Args[0]
	t.Setenv("GO_PROVIDER_FIXTURE", "codex")
	t.Setenv("GO_FIXTURE_HANG", "1")
	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()
	start := time.Now()
	if _, err := m.Refresh(ctx, a); err == nil {
		t.Fatal("hung native process succeeded")
	}
	if time.Since(start) > 2*time.Second {
		t.Fatal("control cancellation did not clean up promptly")
	}
	held, err := config.Lock(a.Profile, "native")
	if err != nil {
		t.Fatal(err)
	}
	defer held()
	t.Setenv("GO_FIXTURE_HANG", "")
	ctx2, cancel2 := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel2()
	if _, err := m.Refresh(ctx2, a); err == nil {
		t.Fatal("cross-process lock was bypassed")
	}
}
