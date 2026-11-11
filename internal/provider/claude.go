package provider

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"sort"
	"strings"
	"time"

	"github.com/amxv/noria/internal/model"
)

// The OAuth usage endpoint is the source of truth for subscription quotas.
// Claude Code's native get_usage can return cached counters for an isolated
// profile, which need not agree with the current claude.ai usage dashboard.
// Refresh expired isolated OAuth credentials under the Refresh profile lock,
// then read the live endpoint without sending a model prompt. Fall back to
// native control only if the account has no readable OAuth credential.
func (m *Manager) claudeQuota(ctx context.Context, a model.Account, p string) (model.Snapshot, error) {
	started := time.Now()
	credential, expired, err := m.readCredential(ctx, a, p)
	if err != nil {
		return m.claudeControlUsage(ctx, a, p, started)
	}
	if expired {
		if err := m.refreshClaudeOAuth(ctx, a, p); err != nil {
			return model.Snapshot{}, fmt.Errorf("Claude usage credential refresh failed; sign in again: %w", err)
		}
		credential, expired, err = m.readCredential(ctx, a, p)
		if err != nil || expired {
			return model.Snapshot{}, errors.New("Claude OAuth credential refresh was not persisted; sign in again")
		}
	}
	oauth, err := m.claudeOAuthUsage(ctx, a.ID, credential.Token, started)
	if errors.Is(err, errClaudeUsageUnauthorized) {
		// A token can be revoked before its expiry. Make exactly one refresh
		// attempt, without sending a model prompt or repeating indefinitely.
		if rotationErr := m.refreshClaudeOAuth(ctx, a, p); rotationErr != nil {
			return model.Snapshot{}, fmt.Errorf("Claude usage authorization failed; sign in again: %w", rotationErr)
		}
		credential, expired, err = m.readCredential(ctx, a, p)
		if err != nil || expired {
			return model.Snapshot{}, errors.New("Claude OAuth credential refresh was not persisted; sign in again")
		}
		oauth, err = m.claudeOAuthUsage(ctx, a.ID, credential.Token, time.Now())
	}
	return oauth, err
}

var errClaudeUsageUnauthorized = errors.New("Claude usage OAuth authorization rejected")

func hasClaudeFableWindow(snapshot model.Snapshot) bool {
	for _, window := range snapshot.Windows {
		if window.Name == "seven_day_fable" && window.Model == "fable" && window.Used != nil {
			return true
		}
	}
	return false
}

func (m *Manager) claudeControlUsage(ctx context.Context, a model.Account, p string, started time.Time) (model.Snapshot, error) {
	cmd := nativeCommand(ctx, append(cleanEnv(p, "claude"),
		"CLAUDE_CODE_ENTRYPOINT=sdk-go", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1"),
		m.claudeCommand,
		"--output-format", "stream-json", "--verbose",
		"--input-format", "stream-json", "--setting-sources=",
		"--tools", "", "--no-session-persistence")
	cmd.Dir = p
	input, err := cmd.StdinPipe()
	if err != nil {
		return model.Snapshot{}, errors.New("cannot open Claude control input")
	}
	output, err := cmd.StdoutPipe()
	if err != nil {
		return model.Snapshot{}, errors.New("cannot open Claude control output")
	}
	if err := cmd.Start(); err != nil {
		return model.Snapshot{}, errors.New("cannot start Claude Code; install the native claude executable")
	}
	defer func() { _ = input.Close(); killProcess(cmd); _ = cmd.Wait() }()

	// Initialize the control session without submitting a user message. Read
	// the successful handshake before asking for usage, matching the CLI's
	// control protocol rather than relying on input queue ordering.
	encoder := json.NewEncoder(input)
	if err := encoder.Encode(map[string]any{
		"request_id": "noria_initialize", "type": "control_request",
		"request": map[string]string{"subtype": "initialize"},
	}); err != nil {
		return model.Snapshot{}, errors.New("Claude control initialization write failed")
	}
	reader := bufio.NewScanner(output)
	reader.Buffer(make([]byte, 4096), 1<<20)
	initialized := false
	for reader.Scan() {
		var message struct {
			Type     string `json:"type"`
			Response struct {
				RequestID string          `json:"request_id"`
				Subtype   string          `json:"subtype"`
				Response  json.RawMessage `json:"response"`
			} `json:"response"`
		}
		if json.Unmarshal(reader.Bytes(), &message) != nil {
			return model.Snapshot{}, errors.New("invalid Claude control message")
		}
		if message.Type != "control_response" {
			// The quota reader must not initiate model turns or tool calls. A
			// system initialization message is fine, but user/model events are not.
			if message.Type == "assistant" || message.Type == "user" || message.Type == "result" || message.Type == "control_request" {
				return model.Snapshot{}, errors.New("unexpected Claude activity during passive quota read")
			}
			continue
		}
		if message.Response.Subtype != "success" {
			return model.Snapshot{}, errors.New("Claude control request was rejected")
		}
		switch message.Response.RequestID {
		case "noria_initialize":
			initialized = true
			// Behaviors are large local usage statistics Noria does not need.
			// The native request parameter preserves the quota fields while
			// avoiding unnecessary work and oversized control responses.
			if err := encoder.Encode(map[string]any{
				"request_id": "noria_usage", "type": "control_request",
				"request": map[string]any{"subtype": "get_usage", "skip_behaviors": true},
			}); err != nil {
				return model.Snapshot{}, errors.New("Claude control usage write failed")
			}
		case "noria_usage":
			if !initialized {
				return model.Snapshot{}, errors.New("Claude usage response preceded initialization")
			}
			if len(message.Response.Response) == 0 {
				return model.Snapshot{}, errors.New("Claude usage control returned no data")
			}
			return parseClaude(a.ID, message.Response.Response, started)
		}
	}
	if ctx.Err() != nil {
		return model.Snapshot{}, fmt.Errorf("Claude quota read cancelled: %w", ctx.Err())
	}
	return model.Snapshot{}, errors.New("Claude usage control closed or changed protocol")
}

// claudeOAuthUsage reads the same account's provider-issued subscription
// windows when the native response lacks Fable or all limits. No native
// credentials or server error bodies are logged or exposed to callers.
func (m *Manager) claudeOAuthUsage(ctx context.Context, id, token string, started time.Time) (model.Snapshot, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, m.claudeUsageURL, nil)
	if err != nil {
		return model.Snapshot{}, errors.New("cannot construct Claude subscription usage request")
	}
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("Anthropic-Beta", "oauth-2025-04-20")
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "claude-code")
	resp, err := m.usageClient.Do(req)
	if err != nil {
		return model.Snapshot{}, errors.New("Claude subscription usage endpoint unavailable")
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		if resp.StatusCode == http.StatusUnauthorized || resp.StatusCode == http.StatusForbidden {
			return model.Snapshot{}, errClaudeUsageUnauthorized
		}
		return model.Snapshot{}, fmt.Errorf("Claude subscription usage returned HTTP %d", resp.StatusCode)
	}
	data, err := io.ReadAll(io.LimitReader(resp.Body, (64<<10)+1))
	if err != nil || len(data) > 64<<10 {
		return model.Snapshot{}, errors.New("invalid Claude subscription usage response size")
	}
	// The HTTP shape is the rate_limits object, whereas the native control
	// envelope additionally carries rate_limits_available.
	var limits map[string]json.RawMessage
	if json.Unmarshal(data, &limits) != nil || limits == nil {
		return model.Snapshot{}, errors.New("invalid Claude subscription usage response")
	}
	wrapped, _ := json.Marshal(map[string]any{"rate_limits_available": true, "rate_limits": limits})
	snapshot, err := parseClaude(id, wrapped, started)
	if err != nil {
		return model.Snapshot{}, err
	}
	if len(snapshot.Windows) == 0 {
		return model.Snapshot{}, errors.New("Claude subscription usage returned no quota windows")
	}
	return snapshot, nil
}

func parseClaude(id string, data []byte, now time.Time) (model.Snapshot, error) {
	s := model.Snapshot{AccountID: id, ObservedAt: now}
	var root struct {
		Available *bool           `json:"rate_limits_available"`
		Limits    json.RawMessage `json:"rate_limits"`
	}
	if json.Unmarshal(data, &root) != nil || root.Available == nil || len(root.Limits) == 0 {
		return s, errors.New("Claude usage schema changed")
	}
	if !*root.Available {
		if !absent(root.Limits) {
			return s, errors.New("invalid unavailable Claude usage schema")
		}
		return s, nil
	}
	if absent(root.Limits) {
		return s, nil
	}
	var limits map[string]json.RawMessage
	if json.Unmarshal(root.Limits, &limits) != nil {
		return s, errors.New("Claude rate limits must be an object or null")
	}
	keys := make([]string, 0, len(limits))
	for key := range limits {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	for _, key := range keys {
		// Provider billing/feature metadata is not a subscription window.
		// Ignore unknown buckets before applying the window schema.
		// Claude's actual OAuth endpoint also returns seven_day_breakdown,
		// which is detailed analytics (rows/as_of), not a model quota.
		if key == "seven_day_breakdown" {
			continue
		}
		if key != "five_hour" && key != "seven_day" && !strings.HasPrefix(key, "five_hour_") && !strings.HasPrefix(key, "seven_day_") {
			continue
		}
		if absent(limits[key]) {
			continue
		}
		var fields map[string]json.RawMessage
		if json.Unmarshal(limits[key], &fields) != nil {
			return s, errors.New("Claude usage window schema changed")
		}
		if len(fields["utilization"]) == 0 || len(fields["resets_at"]) == 0 {
			return s, errors.New("Claude usage window fields missing")
		}
		var used *float64
		var reset *string
		if json.Unmarshal(fields["utilization"], &used) != nil || json.Unmarshal(fields["resets_at"], &reset) != nil {
			return s, errors.New("invalid Claude usage window")
		}
		if used != nil && (*used < 0 || *used > 100) {
			return s, errors.New("Claude utilization outside 0..100")
		}
		w := model.Window{Name: key, LimitID: key, Used: used}
		switch {
		case key == "five_hour":
			w.Duration = 5 * time.Hour
		case key == "seven_day":
			w.Duration = 7 * 24 * time.Hour
		case strings.HasPrefix(key, "seven_day_"):
			w.Duration = 7 * 24 * time.Hour
			w.Model = strings.TrimPrefix(key, "seven_day_")
		case strings.HasPrefix(key, "five_hour_"):
			w.Duration = 5 * time.Hour
			w.Model = strings.TrimPrefix(key, "five_hour_")
		default:
			continue
		}
		if reset != nil {
			v, err := time.Parse(time.RFC3339Nano, *reset)
			if err != nil {
				return s, errors.New("Claude usage reset must be timezone-aware ISO timestamp")
			}
			w.ResetsAt = &v
		}
		s.Windows = append(s.Windows, w)
	}
	// The live OAuth endpoint's limits[] counters supersede the legacy named
	// windows, which can lag behind claude.ai. Never sum or average these two
	// representations. Model-scoped Fable is optional and never plan-inferred.
	if raw := limits["model_scoped"]; !absent(raw) {
		var items []struct {
			DisplayName string          `json:"display_name"`
			Utilization json.RawMessage `json:"utilization"`
			ResetsAt    json.RawMessage `json:"resets_at"`
		}
		if json.Unmarshal(raw, &items) != nil {
			return s, errors.New("invalid Claude model-scoped usage")
		}
		for _, item := range items {
			if !claudeFableName(item.DisplayName) {
				continue
			}
			w, err := claudeFableWindow(item.Utilization, item.ResetsAt)
			if err != nil {
				return s, err
			}
			putClaudeFableWindow(&s, w)
		}
	}
	if raw := limits["limits"]; !absent(raw) {
		var items []struct {
			Kind     string          `json:"kind"`
			Group    string          `json:"group"`
			Percent  json.RawMessage `json:"percent"`
			ResetsAt json.RawMessage `json:"resets_at"`
			Scope    struct {
				Model struct {
					DisplayName string `json:"display_name"`
				} `json:"model"`
			} `json:"scope"`
		}
		if json.Unmarshal(raw, &items) != nil {
			return s, errors.New("invalid Claude scoped limits")
		}
		for _, item := range items {
			var name, modelName string
			switch {
			case item.Kind == "session" && (item.Group == "" || item.Group == "session"):
				name = "five_hour"
			case item.Kind == "weekly_all" && (item.Group == "" || item.Group == "weekly"):
				name = "seven_day"
			case item.Kind == "weekly_scoped" && (item.Group == "" || item.Group == "weekly") && claudeFableName(item.Scope.Model.DisplayName):
				name, modelName = "seven_day_fable", "fable"
			default:
				continue
			}
			w, err := claudeUsageWindow(name, modelName, item.Percent, item.ResetsAt)
			if err != nil {
				return s, err
			}
			putClaudeWindow(&s, w)
		}
	}
	s.Available = true
	return s, nil
}

func claudeFableName(name string) bool {
	name = strings.ToLower(strings.TrimSpace(name))
	return name == "fable" || name == "claude fable" || strings.HasPrefix(name, "fable ") || strings.HasPrefix(name, "claude fable ")
}

func claudeFableWindow(usage, resetAt json.RawMessage) (model.Window, error) {
	return claudeUsageWindow("seven_day_fable", "fable", usage, resetAt)
}

func claudeUsageWindow(name, modelName string, usage, resetAt json.RawMessage) (model.Window, error) {
	w := model.Window{Name: name, LimitID: name, Model: modelName, Duration: 7 * 24 * time.Hour}
	if name == "five_hour" {
		w.Duration = 5 * time.Hour
	}
	if len(usage) == 0 || len(resetAt) == 0 {
		return w, errors.New("Claude Fable weekly usage fields missing")
	}
	var reset *string
	if json.Unmarshal(usage, &w.Used) != nil || json.Unmarshal(resetAt, &reset) != nil {
		return w, errors.New("invalid Claude Fable weekly usage")
	}
	if w.Used != nil && (*w.Used < 0 || *w.Used > 100) {
		return w, errors.New("Claude Fable utilization outside 0..100")
	}
	if reset != nil {
		v, err := time.Parse(time.RFC3339Nano, *reset)
		if err != nil {
			return w, errors.New("Claude Fable reset must be timezone-aware ISO timestamp")
		}
		w.ResetsAt = &v
	}
	return w, nil
}

func putClaudeFableWindow(snapshot *model.Snapshot, window model.Window) {
	putClaudeWindow(snapshot, window)
}

func putClaudeWindow(snapshot *model.Snapshot, window model.Window) {
	for i := range snapshot.Windows {
		if snapshot.Windows[i].Name == window.Name && snapshot.Windows[i].Model == window.Model {
			snapshot.Windows[i] = window
			return
		}
	}
	snapshot.Windows = append(snapshot.Windows, window)
}
