// Package proxy forwards native provider protocols without translating bodies.
package proxy

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"time"

	"github.com/amxv/noria/internal/buildinfo"
	"github.com/amxv/noria/internal/core"
	"github.com/amxv/noria/internal/model"
	"github.com/gorilla/websocket"
)

// Bounded allowance for image-heavy conversation history in native clients.
const maxBody = 256 << 20
const maxPrelude = 256 << 10
const maxAttempts = 16

type Backend interface {
	Credentials(context.Context, model.Account) (model.Credential, error)
	Refresh(context.Context, model.Account) (model.Snapshot, error)
}

type Server struct {
	scheduler      *core.Scheduler
	backend        Backend
	secret         string
	client         *http.Client
	slots          chan struct{}
	mu             sync.Mutex
	sockets        map[*websocket.Conn]struct{}
	closed         bool
	draining       bool
	drainDeadline  time.Time
	activeCalls    int
	refreshCtx     context.Context
	refreshCancel  context.CancelFunc
	refreshing     map[string]bool
	refreshWorkers sync.WaitGroup
	socketWorkers  sync.WaitGroup
}

func New(scheduler *core.Scheduler, backend Backend, secret string) *Server {
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.MaxIdleConns = 128
	transport.MaxIdleConnsPerHost = 32
	transport.MaxConnsPerHost = 128
	transport.ResponseHeaderTimeout = time.Minute
	transport.DisableCompression = true
	refreshCtx, refreshCancel := context.WithCancel(context.Background())
	return &Server{scheduler: scheduler, backend: backend, secret: secret,
		client: &http.Client{Transport: transport, CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }},
		slots:  make(chan struct{}, 128), sockets: make(map[*websocket.Conn]struct{}),
		refreshCtx: refreshCtx, refreshCancel: refreshCancel, refreshing: make(map[string]bool)}
}

// Shutdown closes hijacked WebSockets as well as idle HTTP connections.
func (s *Server) Shutdown(ctx context.Context) error {
	s.mu.Lock()
	s.closed = true
	s.refreshCancel()
	for conn := range s.sockets {
		_ = conn.Close()
	}
	s.mu.Unlock()
	s.client.CloseIdleConnections()
	done := make(chan struct{})
	go func() { s.refreshWorkers.Wait(); s.socketWorkers.Wait(); close(done) }()
	select {
	case <-done:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if !localHost(r.Host) || !validOrigin(r) {
		fail(w, http.StatusForbidden, "only local clients are allowed")
		return
	}
	key := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
	if key == "" {
		key = r.Header.Get("X-Api-Key")
	}
	if s.secret == "" || subtle.ConstantTimeCompare([]byte(key), []byte(s.secret)) != 1 {
		fail(w, http.StatusUnauthorized, "local authentication required")
		return
	}
	s.mu.Lock()
	closed := s.closed
	s.mu.Unlock()
	if closed {
		fail(w, http.StatusServiceUnavailable, "proxy is shutting down")
		return
	}
	if r.Method == http.MethodGet && (r.URL.Path == "/_noria/status" || r.URL.Path == "/status") {
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(s.scheduler.Status())
		return
	}
	if r.Method == http.MethodGet && r.URL.Path == "/_noria/health" {
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{"ok": true, "version": buildinfo.CurrentVersion()})
		return
	}
	if r.Method == http.MethodPost && (r.URL.Path == "/_noria/drain" || r.URL.Path == "/_noria/undrain" || r.URL.Path == "/_noria/resume") {
		s.mu.Lock()
		busy := false
		if r.URL.Path != "/_noria/drain" {
			s.draining = false
		} else if s.activeCalls > 0 {
			busy = true
		} else {
			s.draining = true
			s.drainDeadline = time.Now().Add(30 * time.Second)
		}
		s.mu.Unlock()
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]bool{"busy": busy})
		return
	}
	provider, path := route(r.URL.Path)
	if provider == "" {
		fail(w, http.StatusNotFound, "unknown native API route")
		return
	}
	s.mu.Lock()
	if s.draining && time.Now().After(s.drainDeadline) {
		s.draining = false
	}
	if s.draining || s.closed {
		s.mu.Unlock()
		w.Header().Set("Retry-After", "2")
		fail(w, 503, "router is updating; retry shortly")
		return
	}
	select {
	case s.slots <- struct{}{}:
		s.mu.Unlock()
		defer func() { <-s.slots }()
	default:
		s.mu.Unlock()
		fail(w, http.StatusServiceUnavailable, "connection limit reached")
		return
	}
	if websocket.IsWebSocketUpgrade(r) {
		if provider != "codex" || path != "/responses" {
			fail(w, http.StatusBadRequest, "WebSocket requires the Codex responses route")
			return
		}
		s.serveWebSocket(w, r, path)
		return
	}
	if !s.beginCall() {
		w.Header().Set("Retry-After", "2")
		fail(w, 503, "router is updating; retry shortly")
		return
	}
	defer s.endCall()
	if r.Method != http.MethodPost {
		w.Header().Set("Allow", "POST")
		fail(w, http.StatusMethodNotAllowed, "POST required")
		return
	}
	if r.ContentLength > maxBody {
		fail(w, http.StatusRequestEntityTooLarge, fmt.Sprintf("request body exceeds %d MiB", maxBody>>20))
		return
	}
	body, err := io.ReadAll(http.MaxBytesReader(w, r.Body, maxBody))
	if err != nil {
		var tooLarge *http.MaxBytesError
		if errors.As(err, &tooLarge) {
			fail(w, http.StatusRequestEntityTooLarge, fmt.Sprintf("request body exceeds %d MiB", maxBody>>20))
		} else {
			fail(w, http.StatusBadRequest, "cannot read request")
		}
		return
	}
	var object map[string]json.RawMessage
	if json.Unmarshal(body, &object) != nil || object == nil {
		fail(w, http.StatusBadRequest, "JSON object required")
		return
	}
	s.serveNative(w, r, provider, path, body, object)
}

func localHost(value string) bool {
	host := value
	if h, _, err := net.SplitHostPort(value); err == nil {
		host = h
	}
	host = strings.Trim(host, "[]")
	return strings.EqualFold(host, "localhost") || (net.ParseIP(host) != nil && net.ParseIP(host).IsLoopback())
}

func validOrigin(r *http.Request) bool {
	if r.Header.Get("Origin") == "" {
		return true
	}
	u, err := url.Parse(r.Header.Get("Origin"))
	return err == nil && (u.Scheme == "http" || u.Scheme == "https") && u.User == nil && u.Path == "" && u.RawQuery == "" && u.Fragment == "" && strings.EqualFold(u.Host, r.Host) && localHost(u.Host)
}

func route(path string) (string, string) {
	if strings.HasPrefix(path, "/codex/") {
		provider, native := route(strings.TrimPrefix(path, "/codex"))
		if provider == "codex" {
			return provider, native
		}
		return "", ""
	}
	if strings.HasPrefix(path, "/claude/") {
		provider, native := route(strings.TrimPrefix(path, "/claude"))
		if provider == "claude" {
			return provider, native
		}
		return "", ""
	}
	switch path {
	case "/responses", "/v1/responses", "/backend-api/codex/responses":
		return "codex", "/responses"
	case "/responses/compact", "/v1/responses/compact", "/backend-api/codex/responses/compact":
		return "codex", "/responses/compact"
	case "/messages", "/v1/messages":
		return "claude", "/v1/messages"
	case "/v1/messages/count_tokens":
		return "claude", "/v1/messages/count_tokens"
	}
	return "", ""
}

func sessionKey(r *http.Request, object map[string]json.RawMessage) string {
	parts := []string{}
	for _, key := range []string{"X-Claude-Code-Session-Id", "X-Codex-Session-Id", "Session-Id", "Session_id"} {
		if v := r.Header.Get(key); v != "" {
			parts = append(parts, key, v)
			break
		}
	}
	if len(parts) == 0 {
		for _, key := range []string{"session_id", "prompt_cache_key"} {
			var v string
			_ = json.Unmarshal(object[key], &v)
			if v != "" {
				parts = append(parts, key, v)
				break
			}
		}
	}
	if len(parts) == 0 {
		var metadata map[string]json.RawMessage
		_ = json.Unmarshal(object["metadata"], &metadata)
		var user string
		_ = json.Unmarshal(metadata["user_id"], &user)
		// Native Claude embeds session_id in its JSON user_id on some versions.
		var identity map[string]string
		if json.Unmarshal([]byte(user), &identity) == nil && identity["session_id"] != "" {
			parts = append(parts, "metadata.session_id", identity["session_id"])
		}
	}
	if len(parts) == 0 && r.Header.Get("X-Noria-Session") != "" {
		parts = append(parts, "wrapper", r.Header.Get("X-Noria-Session"))
	}
	if v := r.Header.Get("X-Claude-Code-Agent-Id"); v != "" {
		parts = append(parts, "agent", v)
	}
	if len(parts) == 0 {
		return ""
	}
	encoded, _ := json.Marshal(parts)
	sum := sha256.Sum256(encoded)
	return hex.EncodeToString(sum[:])
}

func randomSession() string {
	var id [16]byte
	_, _ = rand.Read(id[:])
	return hex.EncodeToString(id[:])
}

func modelName(object map[string]json.RawMessage) string {
	var name string
	_ = json.Unmarshal(object["model"], &name)
	return name
}

func endpoint(account model.Account, path, query string) (string, error) {
	base := account.Endpoint
	if base == "" {
		if account.Provider == "codex" {
			base = "https://chatgpt.com/backend-api/codex"
		} else {
			base = "https://api.anthropic.com"
		}
	}
	u, err := url.Parse(base)
	if err != nil || u.Host == "" || (u.Scheme != "http" && u.Scheme != "https") || u.User != nil {
		return "", errors.New("invalid account endpoint")
	}
	if u.Scheme != "https" && !localHost(u.Host) {
		return "", errors.New("non-loopback account endpoints must use HTTPS")
	}
	if u.RawQuery != "" || u.Fragment != "" {
		return "", errors.New("account endpoint cannot contain a query or fragment")
	}
	u.Path = strings.TrimSuffix(u.Path, "/") + path
	u.RawQuery = query
	return u.String(), nil
}

func upstreamHeaders(original http.Header, provider string, credential model.Credential) http.Header {
	h := original.Clone()
	stripHopHeaders(h)
	for _, key := range []string{"Authorization", "X-Api-Key", "Proxy-Authorization", "Cookie", "Chatgpt-Account-Id", "X-Noria-Session", "Origin", "Referer", "Sec-Websocket-Key", "Sec-Websocket-Version", "Sec-Websocket-Extensions", "Sec-Websocket-Protocol"} {
		h.Del(key)
	}
	h.Set("Authorization", "Bearer "+credential.Token)
	if provider == "codex" && credential.AccountID != "" {
		h.Set("Chatgpt-Account-Id", credential.AccountID)
	}
	if provider == "claude" {
		// The downstream CLI authenticates to the local gateway, not OAuth.
		// Restore only the upstream OAuth mode marker; retain all native betas.
		const oauthBeta = "oauth-2025-04-20"
		betas := strings.Join(h.Values("Anthropic-Beta"), ",")
		found := false
		for _, beta := range strings.Split(betas, ",") {
			if strings.TrimSpace(beta) == oauthBeta {
				found = true
			}
		}
		if !found {
			if strings.TrimSpace(betas) != "" {
				betas += ","
			}
			betas += oauthBeta
		}
		h.Set("Anthropic-Beta", betas)
	}
	return h
}

func stripHopHeaders(h http.Header) {
	for _, value := range h.Values("Connection") {
		for _, key := range strings.Split(value, ",") {
			h.Del(strings.TrimSpace(key))
		}
	}
	for _, key := range []string{"Connection", "Proxy-Connection", "Keep-Alive", "Proxy-Authenticate", "Proxy-Authorization", "Te", "Trailer", "Transfer-Encoding", "Upgrade"} {
		h.Del(key)
	}
}

func copyHeaders(dst, src http.Header) {
	h := src.Clone()
	stripHopHeaders(h)
	for _, key := range []string{"Set-Cookie", "Authorization", "X-Api-Key", "Chatgpt-Account-Id"} {
		h.Del(key)
	}
	for k, values := range h {
		dst[k] = append([]string(nil), values...)
	}
}

func fail(w http.ResponseWriter, status int, message string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]any{"error": map[string]string{"type": "noria_error", "message": message}})
}

func unavailableDetails(err error) (map[string]any, time.Time) {
	details := map[string]any{"type": "noria_error", "message": "no usable account; inspect noria status or refresh quota"}
	var unavailable *core.UnavailableError
	if errors.As(err, &unavailable) {
		details["message"] = unavailable.Error()
		if !unavailable.RetryAt.IsZero() {
			details["retry_at"] = unavailable.RetryAt.UTC().Format(time.RFC3339)
		}
		return details, unavailable.RetryAt
	}
	return details, time.Time{}
}

func failUnavailable(w http.ResponseWriter, err error) {
	details, reset := unavailableDetails(err)
	if !reset.IsZero() {
		w.Header().Set("Retry-After", reset.UTC().Format(http.TimeFormat))
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusServiceUnavailable)
	_ = json.NewEncoder(w).Encode(map[string]any{"error": details})
}

func (s *Server) beginCall() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.draining && time.Now().After(s.drainDeadline) {
		s.draining = false
	}
	if s.draining || s.closed {
		return false
	}
	s.activeCalls++
	return true
}
func (s *Server) endCall() { s.mu.Lock(); s.activeCalls--; s.mu.Unlock() }
