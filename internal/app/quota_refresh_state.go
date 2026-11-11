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
	AttemptedAt time.Time `json:"attempted_at"`
	OK          bool      `json:"ok"`
	Error       string    `json:"error,omitempty"`
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
		message := strings.Map(func(r rune) rune {
			if r < 32 || r == 127 {
				return ' '
			}
			return r
		}, result.Error)
		if len(message) > 240 {
			message = message[:240]
		}
		entries[result.Account] = quotaRefreshStatus{AttemptedAt: time.Now(), OK: result.OK, Error: message}
	}
	b, err := json.Marshal(entries)
	if err != nil {
		return err
	}
	return config.Write(filepath.Join(home, "quota-refresh.json"), b)
}
