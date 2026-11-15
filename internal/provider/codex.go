package provider

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"sort"
	"strings"
	"time"

	"github.com/amxv/noria/internal/model"
)

func (m *Manager) codexQuota(ctx context.Context, a model.Account, p string) (model.Snapshot, error) {
	return m.codexControl(ctx, a, p, false)
}

func (m *Manager) codexControl(ctx context.Context, a model.Account, p string, refreshToken bool) (model.Snapshot, error) {
	started := time.Now()
	cmd := nativeCommand(ctx, cleanEnv(p, "codex"), m.codexCommand, "app-server", "-c", `cli_auth_credentials_store="file"`, "-c", `model_provider="openai"`, "-c", `openai_base_url="https://api.openai.com/v1"`)
	cmd.Dir = p
	in, err := cmd.StdinPipe()
	if err != nil {
		return model.Snapshot{}, errors.New("cannot open Codex control input")
	}
	out, err := cmd.StdoutPipe()
	if err != nil {
		return model.Snapshot{}, errors.New("cannot open Codex control output")
	}
	if cmd.Start() != nil {
		return model.Snapshot{}, errors.New("cannot start codex app-server; install Codex CLI")
	}
	defer func() { _ = in.Close(); killProcess(cmd); _ = cmd.Wait() }()
	scan := bufio.NewScanner(out)
	scan.Buffer(make([]byte, 4096), 4<<20)
	enc := json.NewEncoder(in)
	next := 0
	rpc := func(method string, params any) (json.RawMessage, error) {
		next++
		id := next
		if enc.Encode(map[string]any{"id": id, "method": method, "params": params}) != nil {
			return nil, errors.New("Codex control write failed")
		}
		for scan.Scan() {
			var msg struct {
				ID     int             `json:"id"`
				Result json.RawMessage `json:"result"`
				Error  json.RawMessage `json:"error"`
			}
			if json.Unmarshal(scan.Bytes(), &msg) != nil {
				return nil, errors.New("invalid Codex control response")
			}
			if msg.ID != id {
				continue
			}
			if len(msg.Error) > 0 && string(msg.Error) != "null" {
				return nil, fmt.Errorf("Codex %s control failed; verify account login", method)
			}
			if len(msg.Result) == 0 {
				return nil, errors.New("Codex control result missing")
			}
			return msg.Result, nil
		}
		return nil, errors.New("Codex control closed or deadline exceeded")
	}
	if _, err = rpc("initialize", map[string]any{"clientInfo": map[string]string{"name": "noria", "title": "Noria", "version": "0.1.0"}, "capabilities": map[string]bool{"experimentalApi": true}}); err != nil {
		return model.Snapshot{}, err
	}
	if enc.Encode(map[string]any{"method": "initialized", "params": map[string]any{}}) != nil {
		return model.Snapshot{}, errors.New("Codex initialization failed")
	}
	readAccount := func() (model.Snapshot, bool, error) {
		// Passive quota reads must not rotate tokens on every poll. Only an
		// expired credential explicitly requests the native client's refresh flow.
		account, err := rpc("account/read", map[string]bool{"refreshToken": refreshToken})
		if err != nil {
			return model.Snapshot{}, false, err
		}
		var auth struct {
			Account  json.RawMessage `json:"account"`
			Required bool            `json:"requiresOpenaiAuth"`
		}
		if json.Unmarshal(account, &auth) != nil {
			return model.Snapshot{}, false, errors.New("invalid Codex account response")
		}
		if auth.Required && (len(auth.Account) == 0 || string(auth.Account) == "null") {
			return model.Snapshot{AccountID: a.ID, ObservedAt: started, AuthInvalid: true}, true, nil
		}
		return model.Snapshot{}, false, nil
	}
	if refreshToken {
		if snapshot, invalid, err := readAccount(); err != nil || invalid {
			return snapshot, err
		}
	}
	// Ask for rate limits directly: account/read is a separate network round
	// trip, so it only runs to classify a failed read as signed out.
	raw, err := rpc("account/rateLimits/read", map[string]any{})
	if err != nil {
		if refreshToken {
			return model.Snapshot{}, err
		}
		if snapshot, invalid, accountErr := readAccount(); accountErr == nil && invalid {
			return snapshot, nil
		}
		return model.Snapshot{}, err
	}
	return parseCodex(a.ID, raw, started)
}

type codexBucket struct {
	Primary      json.RawMessage `json:"primary"`
	Secondary    json.RawMessage `json:"secondary"`
	LimitName    string          `json:"limitName"`
	Ordinary     *bool           `json:"ordinaryUsageAllowed"`
	Reached      *string         `json:"rateLimitReachedType"`
	NormalModel  string          `json:"normalModelSlug"`
	SpendReached *bool           `json:"spendControlReached"`
}

func parseCodex(id string, data []byte, now time.Time) (model.Snapshot, error) {
	s := model.Snapshot{AccountID: id, ObservedAt: now}
	var root struct {
		RateLimits json.RawMessage            `json:"rateLimits"`
		ByID       map[string]json.RawMessage `json:"rateLimitsByLimitId"`
		Ordinary   *bool                      `json:"ordinaryUsageAllowed"`
	}
	if json.Unmarshal(data, &root) != nil {
		return s, errors.New("invalid Codex quota schema")
	}
	raw := root.ByID["codex"]
	if absent(raw) {
		raw = root.RateLimits
	}
	if absent(raw) {
		return s, errors.New("Codex response contains no canonical quota bucket")
	}
	appendBucket := func(raw json.RawMessage, scope, limitID string) error {
		var b codexBucket
		if json.Unmarshal(raw, &b) != nil {
			return errors.New("invalid Codex quota bucket")
		}
		if scope == "" && root.Ordinary != nil {
			b.Ordinary = root.Ordinary
		}
		blocked := (b.Ordinary != nil && !*b.Ordinary) || (b.Ordinary == nil && b.Reached != nil && *b.Reached != "") || (b.SpendReached != nil && *b.SpendReached)
		var fields map[string]json.RawMessage
		_ = json.Unmarshal(raw, &fields)
		_, reachedReported := fields["rateLimitReachedType"]
		gateAllowed := b.Ordinary
		if gateAllowed == nil && (reachedReported || b.SpendReached != nil && *b.SpendReached) {
			allowed := !blocked
			gateAllowed = &allowed
		}
		var blockedUntil *time.Time
		for _, slot := range []struct {
			name string
			raw  json.RawMessage
		}{{"primary", b.Primary}, {"secondary", b.Secondary}} {
			if absent(slot.raw) {
				continue
			}
			var w struct {
				Used  *float64 `json:"usedPercent"`
				Mins  *int64   `json:"windowDurationMins"`
				Reset *int64   `json:"resetsAt"`
			}
			if json.Unmarshal(slot.raw, &w) != nil {
				return errors.New("invalid Codex quota window")
			}
			if w.Used != nil && (*w.Used < 0 || *w.Used > 100) {
				return errors.New("Codex utilization outside 0..100")
			}
			out := model.Window{Name: slot.name, LimitID: limitID, Model: scope, Used: w.Used}
			// A bucket-wide rejection does not exhaust every window. A
			// five-hour stop must not incorrectly inherit a healthy week's reset.
			if b.Ordinary != nil && *b.Ordinary {
				out.Allowed = b.Ordinary
			}
			if w.Mins != nil {
				if *w.Mins <= 0 || *w.Mins > 525600 {
					return errors.New("invalid Codex quota duration")
				}
				out.Duration = time.Duration(*w.Mins) * time.Minute
				switch *w.Mins {
				case 300:
					out.Name = "five_hour"
				case 10080:
					out.Name = "seven_day"
				}
			}
			if w.Reset != nil {
				if *w.Reset < 0 {
					return errors.New("invalid Codex quota reset")
				}
				v := time.Unix(*w.Reset, 0)
				out.ResetsAt = &v
				if blocked && w.Used != nil && *w.Used >= 100 && (blockedUntil == nil || v.After(*blockedUntil)) {
					blockedUntil = &v
				}
			}
			// Keep Allowed separate: rounded display 100 does not establish a
			// block when ordinary usage is explicitly permitted by the backend.
			s.Windows = append(s.Windows, out)
		}
		if gateAllowed != nil || blocked {
			if (b.SpendReached != nil && *b.SpendReached) || (b.Reached != nil && strings.HasPrefix(*b.Reached, "workspace_")) {
				blockedUntil = nil
			}
			// Keep the independent authorization gate. A later positive gate
			// explicitly replaces a prior rejection even with partial windows.
			s.Windows = append(s.Windows, model.Window{Name: "provider", LimitID: limitID, Model: scope, Exhausted: blocked, Allowed: gateAllowed, ResetsAt: blockedUntil})
		}
		return nil
	}
	if err := appendBucket(raw, "", "codex"); err != nil {
		return s, err
	}
	keys := make([]string, 0, len(root.ByID))
	for key := range root.ByID {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	for _, key := range keys {
		if !(strings.HasPrefix(key, "codex_") || strings.HasPrefix(key, "codex-")) || absent(root.ByID[key]) {
			continue
		}
		var b codexBucket
		if json.Unmarshal(root.ByID[key], &b) != nil {
			return s, errors.New("invalid scoped Codex quota bucket")
		}
		if b.LimitName == "" && b.NormalModel == "" {
			continue
		}
		scope := b.NormalModel
		if scope == "" {
			scope = b.LimitName
		}
		if err := appendBucket(root.ByID[key], scope, key); err != nil {
			return s, err
		}
	}
	s.Available = true
	return s, nil
}

func absent(v json.RawMessage) bool { return len(v) == 0 || string(v) == "null" }
