package provider

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// This subprocess acts as the installed Claude binary and implements only
// its control transport. It rejects any user prompt and any credential
// inherited from the caller. No Node runtime or SDK is involved.
func claudeFixture() {
	p := os.Getenv("CLAUDE_CONFIG_DIR")
	if p == "" || os.Getenv("OPENAI_API_KEY") != "" || os.Getenv("ANTHROPIC_BASE_URL") != "" || os.Getenv("CLAUDE_SECURESTORAGE_CONFIG_DIR") != "" || os.Getenv("CLAUDE_CODE_ENTRYPOINT") != "sdk-go" {
		os.Exit(3)
	}
	args := strings.Join(os.Args[1:], "\x00")
	for _, v := range []string{"--input-format\x00stream-json", "--output-format\x00stream-json", "--setting-sources=", "--tools\x00\x00", "--no-session-persistence"} {
		if !strings.Contains(args, v) {
			os.Exit(4)
		}
	}
	trace, e := os.OpenFile(filepath.Join(p, "claude-calls"), os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0600)
	if e != nil {
		os.Exit(5)
	}
	defer trace.Close()
	mode := os.Getenv("GO_CLAUDE_FIXTURE_MODE")
	reader := bufio.NewScanner(os.Stdin)
	encoder := json.NewEncoder(os.Stdout)
	for reader.Scan() {
		var req struct {
			Type      string `json:"type"`
			RequestID string `json:"request_id"`
			Request   struct {
				Subtype       string `json:"subtype"`
				SkipBehaviors bool   `json:"skip_behaviors"`
			} `json:"request"`
		}
		if json.Unmarshal(reader.Bytes(), &req) != nil || req.Type != "control_request" || (req.Request.Subtype != "initialize" && req.Request.Subtype != "get_usage") {
			os.Exit(6)
		}
		if req.Request.Subtype == "get_usage" && !req.Request.SkipBehaviors {
			os.Exit(7)
		}
		fmt.Fprintln(trace, req.Request.Subtype)
		if mode == "hang" {
			select {}
		}
		if mode == "activity" {
			_ = encoder.Encode(map[string]any{"type": "assistant", "message": map[string]any{"role": "assistant"}})
			continue
		}
		if mode == "rejected" {
			_ = encoder.Encode(map[string]any{"type": "control_response", "response": map[string]any{"request_id": req.RequestID, "subtype": "error", "error": "fixture-sensitive"}})
			continue
		}
		var payload any = map[string]any{}
		if req.Request.Subtype == "get_usage" {
			if mode == "null_limits" {
				payload = map[string]any{"rate_limits_available": true, "rate_limits": nil, "subscription_type": "pro"}
			} else {
				limits := map[string]any{"five_hour": map[string]any{"utilization": 12.5, "resets_at": nil}, "seven_day": map[string]any{"utilization": 45.0, "resets_at": nil}}
				if mode == "model_scoped" {
					limits["model_scoped"] = []map[string]any{{"display_name": "Fable", "utilization": 0, "resets_at": "2026-11-15T15:59:59Z"}}
				}
				payload = map[string]any{"rate_limits_available": true,
					"rate_limits": limits,
					"secret":      "must-not-be-returned"}
			}
		}
		_ = encoder.Encode(map[string]any{"type": "control_response", "response": map[string]any{"request_id": req.RequestID, "subtype": "success", "response": payload}})
	}
}

func TestClaudeOAuthUsagePreferredWithoutStartingNativeProcess(t *testing.T) {
	m, a := fixtureProfile(t, "claude")
	m.claudeCommand = os.Args[0]
	t.Setenv("GO_PROVIDER_FIXTURE", "claude")
	t.Setenv("GO_CLAUDE_FIXTURE_MODE", "null_limits")
	t.Setenv("ANTHROPIC_BASE_URL", "http://proxy.invalid")
	// The native CLI and the HTTPS recovery path use only this profile's
	// credential, never inherited tokens or another account's credentials.
	credential := `{"claudeAiOauth":{"accessToken":"isolated-claude-fixture","expiresAt":4102444800000}}`
	if err := os.WriteFile(filepath.Join(a.Profile, ".credentials.json"), []byte(credential), 0600); err != nil {
		t.Fatal(err)
	}
	var calls int
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		calls++
		if r.Method != http.MethodGet || r.URL.Path != "/api/oauth/usage" || r.Header.Get("Authorization") != "Bearer isolated-claude-fixture" || r.Header.Get("Anthropic-Beta") != "oauth-2025-04-20" || r.Header.Get("X-Api-Key") != "" {
			t.Error("usage request did not use the isolated OAuth credential")
		}
		_, _ = io.WriteString(w, `{"five_hour":{"utilization":0,"resets_at":null},"seven_day":{"utilization":67,"resets_at":"2026-10-24T21:59:59.628557+00:00"},"seven_day_breakdown":{"as_of":"2026-10-20T00:00:00Z","rows":[]},"extra_usage":{"is_enabled":false},"iguana_necktie":{"utilization":98}}`)
	}))
	defer upstream.Close()
	m.claudeUsageURL = upstream.URL + "/api/oauth/usage"
	snapshot, err := m.Refresh(context.Background(), a)
	if err != nil {
		t.Fatal(err)
	}
	if calls != 1 || !snapshot.Available || len(snapshot.Windows) != 2 || snapshot.Windows[0].Name != "five_hour" || *snapshot.Windows[0].Used != 0 || snapshot.Windows[0].ResetsAt != nil || snapshot.Windows[1].Name != "seven_day" || *snapshot.Windows[1].Used != 67 {
		t.Fatalf("Claude usage was not recovered: %+v (calls=%d)", snapshot, calls)
	}
	if want := time.Date(2026, time.October, 24, 21, 59, 59, 628557000, time.UTC); !snapshot.Windows[1].ResetsAt.Equal(want) {
		t.Fatal("weekly reset timestamp changed")
	}
	if _, err := os.Stat(filepath.Join(a.Profile, "claude-calls")); !os.IsNotExist(err) {
		t.Fatalf("live OAuth usage must not start the native control process: %v", err)
	}
}

func TestClaudeOAuthUsageProvidesGeneralAndFableWindows(t *testing.T) {
	m, a := fixtureProfile(t, "claude")
	m.claudeCommand = os.Args[0]
	t.Setenv("GO_PROVIDER_FIXTURE", "claude")
	// The authenticated live usage endpoint is preferred over the native
	// control reader (which may report cached values).
	if err := os.WriteFile(filepath.Join(a.Profile, ".credentials.json"), []byte(`{"claudeAiOauth":{"accessToken":"fixture-access","expiresAt":4102444800000}}`), 0600); err != nil {
		t.Fatal(err)
	}
	calls := 0
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		calls++
		if r.Header.Get("Authorization") != "Bearer fixture-access" {
			t.Error("wrong isolated OAuth token")
		}
		_, _ = io.WriteString(w, `{"five_hour":{"utilization":3,"resets_at":null},"seven_day":{"utilization":0,"resets_at":null},"limits":[{"kind":"weekly_scoped","group":"weekly","percent":0,"resets_at":"2026-11-15T15:59:59Z","scope":{"model":{"display_name":"Fable"}}}]}`)
	}))
	defer upstream.Close()
	m.claudeUsageURL = upstream.URL
	s, err := m.Refresh(context.Background(), a)
	if err != nil || calls != 1 || !s.Available || len(s.Windows) != 3 || !hasClaudeFableWindow(s) {
		t.Fatalf("missing optional Fable usage when native only reports generic windows: %+v %v calls=%d", s, err, calls)
	}
	if s.Windows[0].Used == nil || *s.Windows[0].Used != 3 || s.Windows[1].Used == nil || *s.Windows[1].Used != 0 {
		t.Fatalf("live OAuth general windows were not preserved: %+v", s.Windows)
	}
}

func TestClaudeNativeFableDoesNotRequireOAuth(t *testing.T) {
	m, a := fixtureProfile(t, "claude")
	m.claudeCommand = os.Args[0]
	t.Setenv("GO_PROVIDER_FIXTURE", "claude")
	t.Setenv("GO_CLAUDE_FIXTURE_MODE", "model_scoped")
	s, err := m.Refresh(context.Background(), a)
	if err != nil || !s.Available || len(s.Windows) != 3 || !hasClaudeFableWindow(s) {
		t.Fatalf("Fable reported by native control should need no OAuth credentials: %+v %v", s, err)
	}
}

func TestClaudeOAuthFailureCannotMasqueradeAsFreshNativeUsage(t *testing.T) {
	m, a := fixtureProfile(t, "claude")
	m.claudeCommand = os.Args[0]
	t.Setenv("GO_PROVIDER_FIXTURE", "claude")
	if err := os.WriteFile(filepath.Join(a.Profile, ".credentials.json"), []byte(`{"claudeAiOauth":{"accessToken":"fixture-access","expiresAt":4102444800000}}`), 0600); err != nil {
		t.Fatal(err)
	}
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(503)
		_, _ = io.WriteString(w, `{"secret":"should-not-be-logged"}`)
	}))
	defer upstream.Close()
	m.claudeUsageURL = upstream.URL
	s, err := m.Refresh(context.Background(), a)
	if err == nil || s.Available || len(s.Windows) != 0 || strings.Contains(err.Error(), "should-not-be-logged") {
		t.Fatalf("failed live fetch must not return or leak stale native quotas: %+v %v", s, err)
	}
}

func TestClaudeMissingNativeUsageWithoutCredentialsDoesNotGuessLimits(t *testing.T) {
	m, a := fixtureProfile(t, "claude")
	m.claudeCommand = os.Args[0]
	t.Setenv("GO_PROVIDER_FIXTURE", "claude")
	t.Setenv("GO_CLAUDE_FIXTURE_MODE", "null_limits")
	snapshot, err := m.Refresh(context.Background(), a)
	if err != nil || snapshot.Available || len(snapshot.Windows) != 0 {
		t.Fatalf("unsigned-in profile must remain unknown: %+v %v", snapshot, err)
	}
}

func TestClaudeUsageRecoveryDoesNotExposeServerErrorOrAcceptInvalidData(t *testing.T) {
	m, a := fixtureProfile(t, "claude")
	m.claudeCommand = os.Args[0]
	t.Setenv("GO_PROVIDER_FIXTURE", "claude")
	t.Setenv("GO_CLAUDE_FIXTURE_MODE", "null_limits")
	if err := os.WriteFile(filepath.Join(a.Profile, ".credentials.json"), []byte(`{"claudeAiOauth":{"accessToken":"private-credential"}}`), 0600); err != nil {
		t.Fatal(err)
	}
	for _, tc := range []struct {
		status int
		body   string
	}{
		{403, `{"error":"secret-remote-body"}`},
		{200, `{"five_hour":{"utilization":200,"resets_at":null}}`},
		{200, `{"extra_usage":{"is_enabled":true}}`},
	} {
		upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			w.WriteHeader(tc.status)
			_, _ = io.WriteString(w, tc.body)
		}))
		m.claudeUsageURL = upstream.URL
		_, err := m.Refresh(context.Background(), a)
		upstream.Close()
		if err == nil || strings.Contains(err.Error(), "private-credential") || strings.Contains(err.Error(), "secret-remote-body") {
			t.Fatalf("unsafe usage response handling: %v", err)
		}
	}
}

func TestClaudeQuotaUsesInstalledBinaryNoNodeAndNoUserTurn(t *testing.T) {
	m, a := fixtureProfile(t, "claude")
	m.claudeCommand = os.Args[0]
	t.Setenv("GO_PROVIDER_FIXTURE", "claude")
	t.Setenv("ANTHROPIC_API_KEY", "unsafe-inherited-token")
	t.Setenv("ANTHROPIC_BASE_URL", "http://127.0.0.1:43119")
	t.Setenv("CLAUDE_SECURESTORAGE_CONFIG_DIR", "/must/not/use")
	s, err := m.Refresh(context.Background(), a)
	if err != nil {
		t.Fatal(err)
	}
	if !s.Available || len(s.Windows) != 2 || *s.Windows[0].Used != 12.5 {
		t.Fatalf("direct Claude control returned wrong usage: %+v", s)
	}
	data, err := os.ReadFile(filepath.Join(a.Profile, "claude-calls"))
	if err != nil || strings.TrimSpace(string(data)) != "initialize\nget_usage" {
		t.Fatalf("unexpected Claude control sequence: %q %v", data, err)
	}
}

func TestClaudeQuotaRotatesExpiredOAuthBeforeReadingUsage(t *testing.T) {
	m, a := fixtureProfile(t, "claude")
	m.claudeCommand = os.Args[0]
	t.Setenv("GO_PROVIDER_FIXTURE", "claude")
	credentialFile := filepath.Join(a.Profile, ".credentials.json")
	if err := os.WriteFile(credentialFile, []byte(`{"claudeAiOauth":{"accessToken":"expired-access","refreshToken":"previous-refresh","expiresAt":1,"scopes":["user:inference"]},"preserve":"value"}`), 0600); err != nil {
		t.Fatal(err)
	}
	var tokenCalls, usageCalls int
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/token":
			tokenCalls++
			var body map[string]string
			if json.NewDecoder(r.Body).Decode(&body) != nil || body["refresh_token"] != "previous-refresh" || body["grant_type"] != "refresh_token" {
				t.Error("rotated with an incorrect refresh request")
			}
			_, _ = io.WriteString(w, `{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}`)
		case "/usage":
			usageCalls++
			if r.Header.Get("Authorization") != "Bearer new-access" {
				t.Error("used an expired OAuth access token")
			}
			_, _ = io.WriteString(w, `{"limits":[{"kind":"session","group":"session","percent":0,"resets_at":null},{"kind":"weekly_all","group":"weekly","percent":12,"resets_at":null},{"kind":"weekly_scoped","group":"weekly","percent":23,"resets_at":null,"scope":{"model":{"display_name":"Fable"}}}]}`)
		default:
			t.Errorf("unexpected provider request %s", r.URL.Path)
		}
	}))
	defer upstream.Close()
	m.claudeTokenURL, m.claudeUsageURL = upstream.URL+"/token", upstream.URL+"/usage"
	s, err := m.Refresh(context.Background(), a)
	if err != nil || !s.Available || len(s.Windows) != 3 || tokenCalls != 1 || usageCalls != 1 {
		t.Fatalf("expired credential was not rotated before live usage: %+v err=%v rotation=%d usage=%d", s, err, tokenCalls, usageCalls)
	}
	updated, err := os.ReadFile(credentialFile)
	if err != nil {
		t.Fatal(err)
	}
	var root map[string]json.RawMessage
	var oauth struct {
		Access  string `json:"accessToken"`
		Refresh string `json:"refreshToken"`
	}
	if json.Unmarshal(updated, &root) != nil || json.Unmarshal(root["claudeAiOauth"], &oauth) != nil || oauth.Access != "new-access" || oauth.Refresh != "new-refresh" || string(root["preserve"]) != `"value"` {
		t.Fatal("isolated credential rotation did not preserve the native credential file")
	}
	if _, err := os.Stat(filepath.Join(a.Profile, "claude-calls")); !os.IsNotExist(err) {
		t.Fatal("unexpected native Claude model process")
	}
}

func TestClaudeQuotaRetriesOnceOnRevokedOAuth(t *testing.T) {
	m, a := fixtureProfile(t, "claude")
	if err := os.WriteFile(filepath.Join(a.Profile, ".credentials.json"), []byte(`{"claudeAiOauth":{"accessToken":"revoked-access","refreshToken":"old-refresh","expiresAt":4102444800000}}`), 0600); err != nil {
		t.Fatal(err)
	}
	var gets, refreshes int
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/token" {
			refreshes++
			_, _ = io.WriteString(w, `{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}`)
			return
		}
		gets++
		if r.Header.Get("Authorization") == "Bearer revoked-access" {
			w.WriteHeader(401)
			return
		}
		if r.Header.Get("Authorization") != "Bearer new-access" {
			t.Error("unexpected access token")
		}
		_, _ = io.WriteString(w, `{"seven_day":{"utilization":12,"resets_at":null}}`)
	}))
	defer server.Close()
	m.claudeUsageURL, m.claudeTokenURL = server.URL+"/usage", server.URL+"/token"
	s, err := m.Refresh(context.Background(), a)
	if err != nil || !s.Available || gets != 2 || refreshes != 1 {
		t.Fatalf("revoked token was not rotated exactly once: %+v %v gets=%d refreshes=%d", s, err, gets, refreshes)
	}
}

func TestClaudeControlRejectsUnexpectedModelActivityAndErrors(t *testing.T) {
	for _, mode := range []string{"activity", "rejected"} {
		t.Run(mode, func(t *testing.T) {
			m, a := fixtureProfile(t, "claude")
			m.claudeCommand = os.Args[0]
			t.Setenv("GO_PROVIDER_FIXTURE", "claude")
			t.Setenv("GO_CLAUDE_FIXTURE_MODE", mode)
			_, err := m.Refresh(context.Background(), a)
			if err == nil || strings.Contains(err.Error(), "fixture-sensitive") {
				t.Fatalf("unexpected response must fail without leaking data: %v", err)
			}
		})
	}
}

func TestClaudeControlDeadlineKillsUnresponsiveNativeProcess(t *testing.T) {
	m, a := fixtureProfile(t, "claude")
	m.claudeCommand = os.Args[0]
	t.Setenv("GO_PROVIDER_FIXTURE", "claude")
	t.Setenv("GO_CLAUDE_FIXTURE_MODE", "hang")
	ctx, cancel := context.WithTimeout(context.Background(), 150*time.Millisecond)
	defer cancel()
	started := time.Now()
	_, err := m.Refresh(ctx, a)
	if err == nil || time.Since(started) > 3*time.Second {
		t.Fatalf("Claude control deadline was ignored: %v", err)
	}
}

// Proves real native CLI compatibility without contacting any user's account.
// The isolated, unsigned-in profile returns unavailable limits; no prompts,
// SDK installation, user auth files, or actual model usage are involved.
func TestInstalledClaudeControlWithoutNode(t *testing.T) {
	if os.Getenv("NORIA_NATIVE_COMPAT") != "1" {
		t.Skip("opt-in installed-client compatibility test")
	}
	claude, err := exec.LookPath("claude")
	if err != nil {
		t.Skip("native Claude unavailable")
	}
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("XDG_CONFIG_HOME", home)
	t.Setenv("XDG_CACHE_HOME", home)
	m, a := fixtureProfile(t, "claude")
	m.claudeCommand = claude
	s, err := m.Refresh(context.Background(), a)
	if err != nil {
		t.Fatal(err)
	}
	if s.Available || len(s.Windows) != 0 {
		t.Fatalf("unsigned-in profile unexpectedly yielded active subscription quotas: %+v", s)
	}
	if _, err := os.Stat(filepath.Join(m.home, "bridge")); !os.IsNotExist(err) {
		t.Fatal("native reader unexpectedly needs the old SDK bridge")
	}
}
