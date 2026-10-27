package proxy

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/amxv/noria/internal/core"
	"github.com/amxv/noria/internal/model"
	"github.com/gorilla/websocket"
)

type fakeBackend struct{}

type expiredClaudeBackend struct{ fakeBackend }

func (expiredClaudeBackend) Credentials(context.Context, model.Account) (model.Credential, error) {
	return model.Credential{}, errors.New("native credential refresh failed; run account login: OAuth refresh rejected; sign in again; secret-not-for-client")
}

func TestExpiredClaudeAccountReportsReloginInsteadOfQuota(t *testing.T) {
	server, local := fixture(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		t.Error("unusable Claude credentials must not reach upstream")
	}), "claude", 1)
	// Replace the fixture backend to simulate expired, unrefreshable OAuth.
	// The scheduler still sees an enabled account with no quota blockers.
	// A credential rejection must be distinguishable from an empty quota pool.
	// The synthetic sensitive suffix must never be sent to the client.
	server.backend = expiredClaudeBackend{}
	response := post(t, local.URL+"/claude/v1/messages", `{"model":"claude-haiku-5-5"}`)
	text := read(t, response)
	if response.StatusCode != http.StatusServiceUnavailable || !strings.Contains(text, "sign in again") || strings.Contains(text, "secret-not-for-client") || strings.Contains(text, "refresh quota") {
		t.Fatalf("Claude account auth problem incorrectly reported: %d %s", response.StatusCode, text)
	}
}

func (fakeBackend) Credentials(_ context.Context, account model.Account) (model.Credential, error) {
	return model.Credential{Token: "token-" + account.ID, AccountID: "org-" + account.ID}, nil
}
func (fakeBackend) Refresh(context.Context, model.Account) (model.Snapshot, error) {
	return model.Snapshot{}, errors.New("telemetry unavailable")
}

func fixture(t *testing.T, upstream http.Handler, provider string, count int) (*Server, *httptest.Server) {
	t.Helper()
	remote := httptest.NewServer(upstream)
	t.Cleanup(remote.Close)
	accounts := make([]model.Account, count)
	for i := range accounts {
		accounts[i] = model.Account{ID: string(rune('a' + i)), Provider: provider, Enabled: true, Endpoint: remote.URL}
	}
	scheduler, err := core.New(filepath.Join(t.TempDir(), "state.json"), accounts)
	if err != nil {
		t.Fatal(err)
	}
	server := New(scheduler, fakeBackend{}, "local-secret")
	local := httptest.NewServer(server)
	t.Cleanup(func() { _ = server.Shutdown(context.Background()); local.Close() })
	return server, local
}

func post(t *testing.T, address, body string) *http.Response {
	t.Helper()
	request, err := http.NewRequest(http.MethodPost, address, strings.NewReader(body))
	if err != nil {
		t.Fatal(err)
	}
	request.Header.Set("Authorization", "Bearer local-secret")
	request.Header.Set("Content-Type", "application/json")
	request.Header.Set("Session_id", "conversation")
	response, err := http.DefaultClient.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	return response
}

func read(t *testing.T, response *http.Response) string {
	t.Helper()
	defer response.Body.Close()
	b, err := io.ReadAll(response.Body)
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

func TestNativeBodyHeadersAndQuotaFailover(t *testing.T) {
	body := `{"model":"gpt-test","input":[{"type":"function_call_output","call_id":"c1","output":"verbatim"}],"reasoning":{"effort":"high"}}`
	var attempts atomic.Int32
	_, local := fixture(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		attempts.Add(1)
		got, _ := io.ReadAll(r.Body)
		if string(got) != body {
			t.Error("body changed")
		}
		if r.URL.Path != "/responses" || r.URL.RawQuery != "trace=1" {
			t.Error("route changed")
		}
		if r.Header.Get("Authorization") == "Bearer local-secret" || r.Header.Get("X-Api-Key") != "" || r.Header.Get("X-Noria-Session") != "" {
			t.Error("local authorization leaked")
		}
		if r.Header.Get("Chatgpt-Account-Id") != "org-"+strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer token-") {
			t.Error("account header missing")
		}
		if r.Header.Get("Authorization") == "Bearer token-a" {
			w.WriteHeader(429)
			_, _ = io.WriteString(w, `{"error":{"type":"usage_limit_reached","resets_in_seconds":3600}}`)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = io.WriteString(w, `{"id":"ok","reasoning":"unchanged"}`)
	}), "codex", 2)
	response := post(t, local.URL+"/v1/responses?trace=1", body)
	if response.StatusCode != 200 || read(t, response) != `{"id":"ok","reasoning":"unchanged"}` {
		t.Fatal("request did not fail over")
	}
	if attempts.Load() != 2 {
		t.Fatalf("got %d attempts", attempts.Load())
	}
	response = post(t, local.URL+"/v1/responses?trace=1", body)
	_ = read(t, response)
	if attempts.Load() != 3 {
		t.Fatal("binding did not persist")
	}
}

func TestResumedCodexSessionSwitchesOnGenericSubscriptionLimit(t *testing.T) {
	// The first turn establishes an A binding, as a pre-existing conversation
	// would. A subsequent request is rejected with Codex's generic rate-limit
	// shape. Noria must retry the *same* continuation on B and stay there.
	var limited atomic.Bool
	var aCalls, bCalls atomic.Int32
	_, local := fixture(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		if !strings.Contains(string(body), `"continuation":"retained-history"`) {
			t.Error("continuation body changed during failover")
		}
		if r.Header.Get("Authorization") == "Bearer token-a" {
			aCalls.Add(1)
			if limited.Load() {
				w.WriteHeader(429)
				_, _ = io.WriteString(w, `{"error":{"type":"rate_limit_error","message":"You've hit your usage limit; resets at 8:52 AM."}}`)
				return
			}
		} else {
			bCalls.Add(1)
		}
		_, _ = io.WriteString(w, `{"result":"ok"}`)
	}), "codex", 2)
	body := `{"model":"gpt-test","continuation":"retained-history"}`
	first := post(t, local.URL+"/responses", body)
	if first.StatusCode != 200 {
		t.Fatalf("first turn: %d %s", first.StatusCode, read(t, first))
	}
	read(t, first)
	limited.Store(true)
	second := post(t, local.URL+"/responses", body)
	if second.StatusCode != 200 || !strings.Contains(read(t, second), `"ok"`) {
		t.Fatal("existing session did not fail over")
	}
	third := post(t, local.URL+"/responses", body)
	if third.StatusCode != 200 {
		t.Fatalf("continued binding: %d %s", third.StatusCode, read(t, third))
	}
	read(t, third)
	if aCalls.Load() != 2 || bCalls.Load() != 2 {
		t.Fatalf("expected A initial + rejection, B retry + subsequent turn; A=%d B=%d", aCalls.Load(), bCalls.Load())
	}
}

func TestEarlySSEQuotaAndNoPartialSplice(t *testing.T) {
	for _, partial := range []bool{false, true} {
		t.Run(map[bool]string{false: "early", true: "partial"}[partial], func(t *testing.T) {
			var attempts atomic.Int32
			preamble := "event: response.created\ndata: {\"type\":\"response.created\"}\n\n"
			output := "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n"
			failed := "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"usage_limit_reached\"}}}\n\n"
			_, local := fixture(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				attempts.Add(1)
				w.Header().Set("Content-Type", "text/event-stream")
				_, _ = io.WriteString(w, preamble)
				if r.Header.Get("Authorization") == "Bearer token-a" {
					if partial {
						_, _ = io.WriteString(w, output)
					}
					_, _ = io.WriteString(w, failed)
					return
				}
				_, _ = io.WriteString(w, output+"data: {\"type\":\"response.completed\"}\n\n")
			}), "codex", 2)
			response := post(t, local.URL+"/responses", `{"model":"test","stream":true}`)
			got := read(t, response)
			if partial {
				if attempts.Load() != 1 || got != preamble+output+failed {
					t.Fatal("partial output was retried or changed")
				}
			} else if attempts.Load() != 2 || strings.Contains(got, "usage_limit_reached") || !strings.HasPrefix(got, preamble+output) {
				t.Fatalf("early failover failed: %q", got)
			}
		})
	}
}

func TestNativeClaudeAndThroughputDoesNotFailover(t *testing.T) {
	var attempts atomic.Int32
	body := `{"model":"claude-test","messages":[{"role":"user","content":"hi"}],"max_tokens":5}`
	_, local := fixture(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		attempts.Add(1)
		if r.URL.Path != "/v1/messages" {
			t.Error("bad Claude path")
		}
		if r.Header.Get("Authorization") != "Bearer token-a" || r.Header.Get("Anthropic-Beta") != "oauth-2025-04-20" || r.Header.Get("X-Api-Key") != "" {
			t.Error("native header lost or local auth leaked")
		}
		got, _ := io.ReadAll(r.Body)
		if string(got) != body {
			t.Error("Claude body changed")
		}
		w.WriteHeader(429)
		_, _ = io.WriteString(w, `{"type":"error","error":{"type":"rate_limit_error","message":"too many requests per minute"}}`)
	}), "claude", 2)
	request, _ := http.NewRequest("POST", local.URL+"/v1/messages", strings.NewReader(body))
	request.Header.Set("X-Api-Key", "local-secret")
	request.Header.Set("Anthropic-Beta", "oauth-2025-04-20")
	response, err := http.DefaultClient.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	_ = read(t, response)
	if response.StatusCode != 429 || attempts.Load() != 1 {
		t.Fatal("throughput caused failover")
	}
}

func TestAccessControlsAndMalformedInput(t *testing.T) {
	server, _ := fixture(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(200) }), "codex", 1)
	for _, tc := range []struct {
		name, host, origin, secret, path, body string
		status                                 int
	}{
		{"host", "evil.test", "", "local-secret", "/responses", `{}`, 403},
		{"origin", "127.0.0.1", "https://evil.test", "local-secret", "/responses", `{}`, 403},
		{"port-origin", "127.0.0.1:43119", "http://127.0.0.1:80", "local-secret", "/responses", `{}`, 403},
		{"secret", "127.0.0.1", "", "wrong", "/responses", `{}`, 401},
		{"json", "127.0.0.1", "", "local-secret", "/responses", `null`, 400},
		{"route", "127.0.0.1", "", "local-secret", "/unknown", `{}`, 404},
	} {
		t.Run(tc.name, func(t *testing.T) {
			r := httptest.NewRequest("POST", "http://"+tc.host+tc.path, strings.NewReader(tc.body))
			r.Header.Set("Authorization", "Bearer "+tc.secret)
			r.Header.Set("Origin", tc.origin)
			w := httptest.NewRecorder()
			server.ServeHTTP(w, r)
			if w.Code != tc.status {
				t.Fatalf("got %d", w.Code)
			}
		})
	}
	r := httptest.NewRequest("GET", "http://localhost/_noria/status", nil)
	r.Header.Set("Authorization", "Bearer local-secret")
	w := httptest.NewRecorder()
	server.ServeHTTP(w, r)
	if w.Code != 200 || strings.Contains(w.Body.String(), "token-") || strings.Contains(w.Body.String(), "profile") || strings.Contains(w.Body.String(), "local-secret") {
		t.Fatal("unsafe status")
	}
}

func TestSessionKeysAndBranches(t *testing.T) {
	r := httptest.NewRequest("POST", "http://localhost/responses", nil)
	if sessionKey(r, nil) != "" {
		t.Fatal("unidentified HTTP requests should remain unbound")
	}
	r.Header.Set("X-Noria-Session", "wrapper")
	base := sessionKey(r, nil)
	r.Header.Set("Session_id", "native")
	native := sessionKey(r, nil)
	if base == native {
		t.Fatal("native session ignored")
	}
	r.Header.Set("X-Claude-Code-Agent-Id", "branch")
	if sessionKey(r, nil) == native {
		t.Fatal("agent branch ignored")
	}
	r.Header.Del("Session_id")
	r.Header.Del("X-Claude-Code-Agent-Id")
	object := map[string]json.RawMessage{"prompt_cache_key": json.RawMessage(`"branch-key"`)}
	if sessionKey(r, object) == base {
		t.Fatal("body branch ignored")
	}
}

func TestCancellationReachesUpstream(t *testing.T) {
	started := make(chan struct{})
	cancelled := make(chan struct{})
	_, local := fixture(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.Copy(io.Discard, r.Body)
		close(started)
		<-r.Context().Done()
		close(cancelled)
	}), "codex", 1)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	r, _ := http.NewRequestWithContext(ctx, "POST", local.URL+"/responses", strings.NewReader(`{}`))
	r.Header.Set("Authorization", "Bearer local-secret")
	done := make(chan struct{})
	go func() {
		response, _ := http.DefaultClient.Do(r)
		if response != nil {
			_ = response.Body.Close()
		}
		close(done)
	}()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("upstream not started")
	}
	cancel()
	select {
	case <-cancelled:
	case <-time.After(time.Second):
		t.Fatal("upstream not cancelled")
	}
	<-done
}

func TestWebSocketEarlyFailoverAndConnectionReuse(t *testing.T) {
	var attempts atomic.Int32
	create := `{"type":"response.create","model":"test","input":[{"role":"user","content":"hi"}]}`
	completed := `{"type":"response.completed","response":{"id":"r1"}}`
	_, local := fixture(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		u := websocket.Upgrader{CheckOrigin: func(*http.Request) bool { return true }}
		c, err := u.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		defer c.Close()
		attempts.Add(1)
		for {
			_, body, err := c.ReadMessage()
			if err != nil {
				return
			}
			if string(body) != create {
				t.Error("WS body changed")
			}
			_ = c.WriteMessage(websocket.TextMessage, []byte(`{"type":"response.created"}`))
			if r.Header.Get("Authorization") == "Bearer token-a" {
				_ = c.WriteMessage(websocket.TextMessage, []byte(`{"type":"error","status":429,"error":{"type":"usage_limit_reached"}}`))
				return
			}
			_ = c.WriteMessage(websocket.TextMessage, []byte(completed))
		}
	}), "codex", 2)
	h := http.Header{"Authorization": []string{"Bearer local-secret"}, "Session_id": []string{"ws-session"}}
	c, _, err := websocket.DefaultDialer.Dial(strings.Replace(local.URL, "http://", "ws://", 1)+"/v1/responses", h)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	_ = c.SetReadDeadline(time.Now().Add(3 * time.Second))
	for range 2 {
		if c.WriteMessage(websocket.TextMessage, []byte(create)) != nil {
			t.Fatal("write failed")
		}
		_, first, err := c.ReadMessage()
		if err != nil {
			t.Fatal(err)
		}
		if string(first) != `{"type":"response.created"}` {
			t.Fatalf("unexpected lifecycle %s", first)
		}
		_, last, err := c.ReadMessage()
		if err != nil || string(last) != completed {
			t.Fatalf("completion changed %s, %v", last, err)
		}
	}
	if attempts.Load() != 2 {
		t.Fatalf("upstream healthy connection was not reused: %d", attempts.Load())
	}
}

func TestClassifierScopePermissionAndSubscriptionHeaders(t *testing.T) {
	for _, tc := range []struct {
		body, kind, scope string
		status            int
		retry             bool
	}{
		{`{"error":{"type":"usage_limit_reached"}}`, "quota", "", 429, true},
		{`{"error":{"type":"model_limit_reached"}}`, "quota", "test-model", 429, true},
		{`{"error":{"type":"permission_error"}}`, "", "", 403, false},
		{`{"error":{"type":"authentication_error"}}`, "auth", "", 403, true},
		{`{"error":{"type":"rate_limit_error"}}`, "throughput", "", 429, false},
	} {
		rejection, retry := classify(tc.status, nil, []byte(tc.body), "test-model")
		if rejection.Kind != tc.kind || rejection.Model != tc.scope || retry != tc.retry {
			t.Fatalf("bad classification for %s: %+v %v", tc.body, rejection, retry)
		}
	}
	h := make(http.Header)
	h.Set("Anthropic-Ratelimit-Unified-5h-Status", "rejected")
	h.Set("Anthropic-Ratelimit-Unified-5h-Reset", "2000000000")
	rejection, retry := classify(429, h, []byte(`{"error":{"type":"rate_limit_error"}}`), "test-model")
	if !retry || rejection.Kind != "quota" || rejection.Model != "" || rejection.Until.Unix() != 2000000000 {
		t.Fatalf("native Claude subscription rejection ignored: %+v", rejection)
	}
}

func TestAcceptancePersistenceFailureStopsBeforeStreamCommit(t *testing.T) {
	directory := filepath.Join(t.TempDir(), "state")
	if err := os.Mkdir(directory, 0700); err != nil {
		t.Fatal(err)
	}
	remote := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if err := os.RemoveAll(directory); err != nil {
			t.Error(err)
		}
		if err := os.WriteFile(directory, []byte("prevent state persistence"), 0600); err != nil {
			t.Error(err)
		}
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, "data: {\"type\":\"response.output_text.delta\",\"delta\":\"must not commit\"}\n\n")
	}))
	defer remote.Close()
	scheduler, err := core.New(filepath.Join(directory, "scheduler.json"), []model.Account{{ID: "a", Provider: "codex", Enabled: true, Endpoint: remote.URL}})
	if err != nil {
		t.Fatal(err)
	}
	gateway := New(scheduler, fakeBackend{}, "local-secret")
	defer gateway.Shutdown(context.Background())
	local := httptest.NewServer(gateway)
	defer local.Close()
	response := post(t, local.URL+"/responses", `{}`)
	body := read(t, response)
	if response.StatusCode != 503 || strings.Contains(body, "must not commit") {
		t.Fatalf("unsafe persistence failure: %d %s", response.StatusCode, body)
	}
}

func TestOversizedBodyRejected(t *testing.T) {
	server, _ := fixture(t, http.HandlerFunc(func(http.ResponseWriter, *http.Request) { t.Error("oversized body reached upstream") }), "codex", 1)
	r := httptest.NewRequest("POST", "http://localhost/responses", strings.NewReader(`{}`))
	r.ContentLength = maxBody + 1
	r.Header.Set("Authorization", "Bearer local-secret")
	w := httptest.NewRecorder()
	server.ServeHTTP(w, r)
	if w.Code != 413 {
		t.Fatalf("got %d", w.Code)
	}
}

func TestSSEFlushesBeforeUpstreamFinishes(t *testing.T) {
	finish := make(chan struct{})
	var once sync.Once
	release := func() { once.Do(func() { close(finish) }) }
	defer release()
	_, local := fixture(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, "data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\n")
		w.(http.Flusher).Flush()
		select {
		case <-finish:
		case <-r.Context().Done():
		}
		_, _ = io.WriteString(w, "data: {\"type\":\"response.completed\"}\n\n")
	}), "codex", 1)
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	r, _ := http.NewRequestWithContext(ctx, "POST", local.URL+"/responses", strings.NewReader(`{}`))
	r.Header.Set("Authorization", "Bearer local-secret")
	response, err := http.DefaultClient.Do(r)
	if err != nil {
		release()
		t.Fatal(err)
	}
	defer response.Body.Close()
	line, err := bufio.NewReader(response.Body).ReadString('\n')
	release()
	if err != nil || !strings.Contains(line, "first") {
		t.Fatalf("stream did not flush: %q %v", line, err)
	}
}

func TestWebSocketShutdownClosesHijackedConnection(t *testing.T) {
	server, local := fixture(t, http.HandlerFunc(func(http.ResponseWriter, *http.Request) { t.Error("no generation should start") }), "codex", 1)
	c, _, err := websocket.DefaultDialer.Dial(strings.Replace(local.URL, "http://", "ws://", 1)+"/responses", http.Header{"Authorization": []string{"Bearer local-secret"}})
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	if server.Shutdown(context.Background()) != nil {
		t.Fatal("shutdown failed")
	}
	_ = c.SetReadDeadline(time.Now().Add(time.Second))
	if _, _, err := c.ReadMessage(); err == nil {
		t.Fatal("hijacked connection survived shutdown")
	}
}

func TestClaudeEarlyLifecycleQuotaFailover(t *testing.T) {
	var attempts atomic.Int32
	start := "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"content\":[]}}\n\n"
	stop := "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
	_, local := fixture(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		attempts.Add(1)
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, start)
		if r.Header.Get("Authorization") == "Bearer token-a" {
			_, _ = io.WriteString(w, "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"You have reached your 5-hour usage limit\"}}\n\n")
			return
		}
		_, _ = io.WriteString(w, stop)
	}), "claude", 2)
	response := post(t, local.URL+"/claude/v1/messages", `{"model":"claude-test","stream":true}`)
	if got := read(t, response); got != start+stop || attempts.Load() != 2 {
		t.Fatalf("Claude early quota did not retry: %q", got)
	}
}

func TestWebSocketNativePrewarmDoesNotStartUsageClocks(t *testing.T) {
	var connections atomic.Int32
	server, local := fixture(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		u := websocket.Upgrader{CheckOrigin: func(*http.Request) bool { return true }}
		c, err := u.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		defer c.Close()
		connections.Add(1)
		for {
			_, body, err := c.ReadMessage()
			if err != nil {
				return
			}
			if !json.Valid(body) {
				t.Error("body changed")
			}
			_ = c.WriteMessage(websocket.TextMessage, []byte(`{"type":"response.created"}`))
			_ = c.WriteMessage(websocket.TextMessage, []byte(`{"type":"response.completed"}`))
		}
	}), "codex", 2)
	zero := 0.0
	for _, account := range []string{"a", "b"} {
		if err := server.scheduler.Update(model.Snapshot{AccountID: account, ObservedAt: time.Now(), Available: true, Windows: []model.Window{{Name: "five_hour", Used: &zero, Duration: 5 * time.Hour, Dormant: true}}}); err != nil {
			t.Fatal(err)
		}
	}
	c, _, err := websocket.DefaultDialer.Dial(strings.Replace(local.URL, "http://", "ws://", 1)+"/responses", http.Header{"Authorization": []string{"Bearer local-secret"}})
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	_ = c.SetReadDeadline(time.Now().Add(2 * time.Second))
	for index, create := range []string{`{"type":"response.create","model":"test","generate":false}`, `{"type":"response.create","model":"test","input":[{"role":"user","content":"hi"}],"previous_response_id":"warmup-id"}`} {
		if err := c.WriteMessage(websocket.TextMessage, []byte(create)); err != nil {
			t.Fatal(err)
		}
		for range 2 {
			if _, _, err := c.ReadMessage(); err != nil {
				t.Fatal(err)
			}
		}
		b, _ := json.Marshal(server.scheduler.Status())
		var status struct {
			Bindings int `json:"bindings"`
		}
		_ = json.Unmarshal(b, &status)
		if status.Bindings != index {
			t.Fatalf("prewarm must not accept work; status %s", b)
		}
		if strings.Contains(string(b), "predicted_resets") != (index == 1) {
			t.Fatalf("non-generating prewarm started clocks: %s", b)
		}
	}
	if connections.Load() != 1 {
		t.Fatal("prewarm account or connection changed")
	}
}

func TestUnavailableExposesOnlyRealPoolForecast(t *testing.T) {
	for _, reset := range []time.Time{time.Time{}, time.Now().Add(time.Hour).Truncate(time.Second)} {
		w := httptest.NewRecorder()
		failUnavailable(w, &core.UnavailableError{Provider: "codex", RetryAt: reset})
		if w.Code != 503 {
			t.Fatal("wrong unavailable status")
		}
		if reset.IsZero() {
			if w.Header().Get("Retry-After") != "" || strings.Contains(w.Body.String(), "retry_at") {
				t.Fatal("invented reset for indefinite blocker")
			}
		} else {
			parsed, err := http.ParseTime(w.Header().Get("Retry-After"))
			if err != nil || !parsed.Equal(reset) || !strings.Contains(w.Body.String(), reset.UTC().Format(time.RFC3339)) {
				t.Fatalf("missing forecast: %s", w.Body.String())
			}
		}
	}
}
