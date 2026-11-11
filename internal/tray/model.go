package tray

import (
	"encoding/json"
	"fmt"
	"math"
	"strings"
	"time"
)

type Dashboard struct {
	SchemaVersion int           `json:"schema_version"`
	Version       string        `json:"version"`
	Running       bool          `json:"running"`
	Accounts      []Account     `json:"accounts"`
	ClientRoots   []ClientRoot  `json:"client_roots"`
	Integrations  []Integration `json:"integrations"`
	Preferences   Preferences   `json:"preferences"`
}
type Preferences struct {
	StartAtLogin bool `json:"start_at_login"`
	TrayAtLogin  bool `json:"tray_at_login"`
	AutoUpdate   bool `json:"auto_update"`
}
type Account struct {
	ID       string    `json:"id"`
	Email    string    `json:"email"`
	Alias    string    `json:"alias"`
	Provider string    `json:"provider"`
	Enabled  bool      `json:"enabled"`
	Snapshot *Snapshot `json:"snapshot"`
}

func (a Account) Title() string {
	if a.Alias != "" {
		return a.Alias
	}
	if a.Email != "" {
		return a.Email
	}
	return "Sign-in needed"
}

type Snapshot struct {
	ObservedAt  time.Time     `json:"observed_at"`
	Available   bool          `json:"available"`
	AuthInvalid bool          `json:"auth_invalid"`
	Windows     []QuotaWindow `json:"windows"`
}
type QuotaWindow struct {
	Name        string     `json:"name"`
	Model       string     `json:"model"`
	UsedPercent *float64   `json:"used_percent"`
	Duration    int64      `json:"duration"`
	ResetsAt    *time.Time `json:"resets_at"`
}
type ClientRoot struct {
	Provider string `json:"provider"`
	Root     string `json:"root"`
}
type Integration struct {
	Provider string `json:"provider"`
	Path     string `json:"path"`
	Active   bool   `json:"active"`
	Healthy  bool   `json:"healthy"`
}
type UpdateResult struct {
	Current   string `json:"current"`
	Latest    string `json:"latest"`
	Available bool   `json:"available"`
	Updated   bool   `json:"updated"`
	Deferred  bool   `json:"deferred"`
	Message   string `json:"message"`
}
type HealthReport struct {
	Checks []struct {
		Name   string `json:"name"`
		OK     bool   `json:"ok"`
		Detail string `json:"detail"`
	} `json:"checks"`
}

func ParseDashboard(data []byte) (Dashboard, error) {
	var d Dashboard
	if err := json.Unmarshal(data, &d); err != nil {
		return d, err
	}
	if d.SchemaVersion != 1 {
		return d, fmt.Errorf("unsupported dashboard schema %d", d.SchemaVersion)
	}
	return d, nil
}

// Match Swift's quota rows. Show the optional Fable weekly bucket only when
// the Claude account reports it; a plan name is not evidence of eligibility.
func (a Account) VisibleWindows() []QuotaRow {
	if a.Snapshot == nil {
		return nil
	}
	rows := make([]QuotaRow, 0, 3)
	targets := []struct {
		label    string
		duration time.Duration
		model    string
	}{
		{"5h", 5 * time.Hour, ""}, {"Week", 7 * 24 * time.Hour, ""},
	}
	if a.Provider == "claude" {
		targets = append(targets, struct {
			label    string
			duration time.Duration
			model    string
		}{"Fable Week", 7 * 24 * time.Hour, "fable"})
	}
	for _, target := range targets {
		for _, w := range a.Snapshot.Windows {
			if w.Duration != int64(target.duration) || w.Model != target.model ||
				(target.model == "fable" && w.Name != "seven_day_fable") || w.UsedPercent == nil ||
				math.IsNaN(*w.UsedPercent) || math.IsInf(*w.UsedPercent, 0) || *w.UsedPercent < 0 || *w.UsedPercent > 100 {
				continue
			}
			rows = append(rows, QuotaRow{Title: target.label, Window: w})
			break
		}
	}
	return rows
}

type QuotaRow struct {
	Title  string
	Window QuotaWindow
}

func (r QuotaRow) Text(now time.Time) string {
	left := fmt.Sprintf("%.0f%% left", 100-*r.Window.UsedPercent)
	if r.Window.ResetsAt == nil {
		return r.Title + "  " + left + " · reset unknown"
	}
	remaining := r.Window.ResetsAt.Sub(now)
	if remaining <= 0 {
		return r.Title + "  " + left + " · refreshing reset"
	}
	minutes := int(math.Ceil(remaining.Minutes()))
	countdown := fmt.Sprintf("%dm", minutes)
	if minutes >= 1440 {
		countdown = fmt.Sprintf("%dd %dh", minutes/1440, minutes%1440/60)
	} else if minutes >= 60 {
		countdown = fmt.Sprintf("%dh %dm", minutes/60, minutes%60)
	}
	return r.Title + "  " + left + " · resets in " + countdown
}

func (d Dashboard) Providers() []string {
	providers := make([]string, 0, 2)
	for _, provider := range []string{"codex", "claude"} {
		for _, a := range d.Accounts {
			if a.Provider == provider {
				providers = append(providers, provider)
				break
			}
		}
	}
	return providers
}
func ProviderName(provider string) string {
	if provider == "codex" {
		return "Codex"
	}
	if provider == "claude" {
		return "Claude Code"
	}
	return strings.TrimSpace(provider)
}
