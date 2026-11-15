package app

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/amxv/noria/internal/config"
)

// quotaRefreshStatus is diagnostic metadata only. Snapshots and admission
// remain owned by the scheduler. Never write OAuth tokens or provider bodies.
type quotaRefreshStatus struct {
	AttemptedAt time.Time  `json:"attempted_at"`
	OK          bool       `json:"ok"`
	Error       string     `json:"error,omitempty"`
	RateLimited bool       `json:"rate_limited,omitempty"`
	RetryAt     *time.Time `json:"retry_at,omitempty"`
}

// refreshDebounce skips a provider read when the previous successful read is
// this recent, so rapid menu opens do not exhaust provider usage limits.
const refreshDebounce = 30 * time.Second

// refreshDeferral reports why an account's read should be skipped now:
// a provider cooldown after HTTP 429, or a very recent successful read.
func refreshDeferral(last quotaRefreshStatus, found bool, now time.Time) string {
	if !found {
		return ""
	}
	if last.RetryAt != nil && now.Before(*last.RetryAt) {
		return "rate_limited"
	}
	if last.OK && now.Sub(last.AttemptedAt) >= 0 && now.Sub(last.AttemptedAt) < refreshDebounce {
		return "recent"
	}
	return ""
}

func readQuotaRefreshStatus(home string) map[string]quotaRefreshStatus {
	b, err := os.ReadFile(filepath.Join(home, "quota-refresh.json"))
	if err != nil {
		return nil
	}
	var entries map[string]quotaRefreshStatus
	if json.Unmarshal(b, &entries) != nil {
		return nil
	}
	return entries
}

func recordQuotaRefresh(home string, results []refreshResult) error {
	unlock, err := config.Lock(home, "quota-refresh")
	if err != nil {
		return err
	}
	defer unlock()
	entries := readQuotaRefreshStatus(home)
	if entries == nil {
		entries = make(map[string]quotaRefreshStatus)
	}
	for _, result := range results {
		// A deferred read made no provider request; keep the prior outcome
		// (including an active rate-limit cooldown) unchanged.
		if result.Deferred != "" {
			continue
		}
		message := strings.Map(func(r rune) rune {
			if r < 32 || r == 127 {
				return ' '
			}
			return r
		}, result.Error)
		if len(message) > 240 {
			message = message[:240]
		}
		entries[result.Account] = quotaRefreshStatus{AttemptedAt: time.Now(), OK: result.OK, Error: message,
			RateLimited: result.RetryAt != nil, RetryAt: result.RetryAt}
	}
	b, err := json.Marshal(entries)
	if err != nil {
		return err
	}
	return config.Write(filepath.Join(home, "quota-refresh.json"), b)
}
