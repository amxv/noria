package provider

import (
	"net/http"
	"strconv"
	"strings"
	"time"
)

// Default and bounds for backing off after a provider rate-limits usage
// reads. Retry-After is honored inside these bounds.
const (
	usageRateLimitDefault = 5 * time.Minute
	usageRateLimitMin     = 30 * time.Second
	usageRateLimitMax     = time.Hour
)

// RateLimitError reports that the provider rejected a usage read with HTTP
// 429. Callers must not read usage for that account again before Until.
type RateLimitError struct {
	Provider string
	Until    time.Time
}

func (e *RateLimitError) Error() string {
	return e.Provider + " is rate limiting usage checks; retrying at " + e.Until.Local().Format("15:04")
}

func usageRetryAt(header http.Header, now time.Time) time.Time {
	wait := usageRateLimitDefault
	if value := strings.TrimSpace(header.Get("Retry-After")); value != "" {
		if seconds, err := strconv.Atoi(value); err == nil && seconds > 0 {
			wait = time.Duration(seconds) * time.Second
		} else if at, err := http.ParseTime(value); err == nil {
			wait = at.Sub(now)
		}
	}
	wait = max(usageRateLimitMin, min(usageRateLimitMax, wait))
	return now.Add(wait)
}
