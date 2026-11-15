// Package model defines the shared account and quota vocabulary.
package model

import "time"

type Account struct {
	ID            string    `json:"id"`
	Email         string    `json:"email,omitempty"`
	Alias         string    `json:"alias,omitempty"`
	Authenticated bool      `json:"authenticated,omitempty"`
	LastLogin     time.Time `json:"last_login,omitempty"`
	Provider      string    `json:"provider"`
	Profile       string    `json:"profile"`
	Enabled       bool      `json:"enabled"`
	Endpoint      string    `json:"endpoint,omitempty"`
}

type Window struct {
	Name      string        `json:"name"`
	LimitID   string        `json:"limit_id,omitempty"`
	Model     string        `json:"model,omitempty"`
	Used      *float64      `json:"used_percent,omitempty"`
	ResetsAt  *time.Time    `json:"resets_at,omitempty"`
	Duration  time.Duration `json:"duration,omitempty"`
	Exhausted bool          `json:"exhausted,omitempty"`
	Allowed   *bool         `json:"ordinary_usage_allowed,omitempty"`
	Dormant   bool          `json:"awaiting_first_use,omitempty"`
}

type Snapshot struct {
	AccountID   string    `json:"account_id"`
	ObservedAt  time.Time `json:"observed_at"`
	Available   bool      `json:"available"`
	AuthInvalid bool      `json:"auth_invalid,omitempty"`
	// Complete marks a read that lists every window the provider currently
	// enforces. Windows absent from a complete read were removed upstream
	// (for example, a retired five-hour limit) and must not be carried over.
	Complete bool     `json:"complete,omitempty"`
	Windows  []Window `json:"windows,omitempty"`
}

type Credential struct {
	Token     string
	AccountID string
}

type Rejection struct {
	Kind   string // quota, auth, throughput, transient
	Model  string
	Window string
	Until  time.Time
}

// DisplayName never requires a user-supplied alias. IDs remain stable for routing.
func (a Account) DisplayName() string {
	if a.Alias != "" {
		return a.Alias
	}
	if a.Email != "" {
		return a.Email
	}
	return a.ID
}

// Label is presentation only; immutable IDs retain session affinity across renames.
func (a Account) Label() string {
	if a.Alias != "" {
		return a.Alias
	}
	if a.Email != "" {
		return a.Email
	}
	return "Sign-in required"
}
