package provider

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestUsageRetryAtHonorsRetryAfterWithinBounds(t *testing.T) {
	now := time.Date(2026, 11, 15, 12, 0, 0, 0, time.UTC)
	cases := map[string]time.Duration{
		"":     usageRateLimitDefault,
		"120":  2 * time.Minute,
		"1":    usageRateLimitMin,
		"9999": usageRateLimitMax,
		"junk": usageRateLimitDefault,
		now.Add(10 * time.Minute).Format(http.TimeFormat): 10 * time.Minute,
	}
	for value, want := range cases {
		header := http.Header{}
		if value != "" {
			header.Set("Retry-After", value)
		}
		if got := usageRetryAt(header, now).Sub(now); got != want {
			t.Errorf("Retry-After %q: got %v want %v", value, got, want)
		}
	}
}

func TestClaudeUsage429ReturnsRateLimitError(t *testing.T) {
	m, a := fixtureProfile(t, "claude")
	if err := os.WriteFile(filepath.Join(a.Profile, ".credentials.json"), []byte(`{"claudeAiOauth":{"accessToken":"fixture-access","expiresAt":4102444800000}}`), 0600); err != nil {
		t.Fatal(err)
	}
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Retry-After", "90")
		w.WriteHeader(http.StatusTooManyRequests)
	}))
	defer upstream.Close()
	m.claudeUsageURL = upstream.URL
	started := time.Now()
	_, err := m.Refresh(context.Background(), a)
	var limited *RateLimitError
	if !errors.As(err, &limited) {
		t.Fatalf("429 not reported as a rate limit: %v", err)
	}
	if wait := limited.Until.Sub(started); wait < 89*time.Second || wait > 95*time.Second {
		t.Fatalf("Retry-After not honored: %v", wait)
	}
}
