package core

import (
	"fmt"
	"math"
	"time"

	"github.com/amxv/noria/internal/model"
)

// Update installs a timestamped read. A read started before a rejection cannot
// clear that rejection, even when the read finishes later.
func (s *Scheduler) Update(snapshot model.Snapshot) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	id := snapshot.AccountID
	if id == "" {
		return fmt.Errorf("snapshot account ID is required")
	}
	if snapshot.ObservedAt.IsZero() {
		return fmt.Errorf("snapshot observation time is required")
	}
	if snapshot.ObservedAt.After(s.now().Add(time.Minute)) {
		return fmt.Errorf("snapshot observation time is in the future")
	}
	for _, w := range snapshot.Windows {
		if w.Duration < 0 {
			return fmt.Errorf("invalid negative quota duration")
		}
		if w.Used != nil && (math.IsNaN(*w.Used) || math.IsInf(*w.Used, 0) || *w.Used < 0 || *w.Used > 100) {
			return fmt.Errorf("quota utilization outside 0..100")
		}
	}
	oldSnapshot, existed := s.state.Snapshots[id]
	if existed && !snapshot.ObservedAt.After(oldSnapshot.ObservedAt) {
		return nil
	}
	// Null telemetry does not erase an authoritative exhausted snapshot. It does
	// make the observation unknown, while blockers remain independent.
	oldBlocks := s.state.Blockers[id]
	oldLearning := s.state.Learning[id]
	oldClocks := s.state.Clocks[id]
	oldObserved := s.state.WindowObserved[id]
	learned := map[string]learning{}
	for k, v := range oldLearning {
		learned[k] = v
	}
	clocks := map[string]time.Time{}
	for k, v := range oldClocks {
		clocks[k] = v
	}
	observed := map[string]time.Time{}
	for k, v := range oldObserved {
		observed[k] = v
	}
	present := map[string]bool{}
	for _, w := range snapshot.Windows {
		k := windowKey(w)
		present[k] = true
		observed[k] = snapshot.ObservedAt
		if w.ResetsAt != nil {
			delete(clocks, k)
		}
		if w.Used == nil {
			continue
		}
		l, ok := learned[k]
		completed := s.completedFor(id, w.Model)
		sameEpoch := (l.ResetAt == nil && w.ResetsAt == nil) ||
			(l.ResetAt != nil && w.ResetsAt != nil && l.ResetAt.Equal(*w.ResetsAt))
		if ok && sameEpoch && completed > l.Completed && *w.Used > l.Used {
			delta := (*w.Used - l.Used) / float64(completed-l.Completed)
			// A window reset is detected by a decrease, so it never becomes a
			// negative sample. Each window learns its own percent/work scale.
			if l.Samples == 0 {
				l.Mean = delta
			} else {
				l.Mean = .7*l.Mean + .3*delta
			}
			l.Samples++
		}
		l.Used, l.Completed, l.ObservedAt = *w.Used, completed, snapshot.ObservedAt
		l.ResetAt = nil
		if w.ResetsAt != nil {
			reset := *w.ResetsAt
			l.ResetAt = &reset
		}
		learned[k] = l
	}
	blocks := make([]blocker, 0, len(oldBlocks))
	for _, b := range oldBlocks {
		if !s.snapshotClears(snapshot, b) {
			blocks = append(blocks, b)
		}
	}
	for _, w := range snapshot.Windows {
		if windowExhausted(w) {
			b := blocker{Kind: "quota", Source: "telemetry", Model: w.Model, Window: w.Name, CreatedAt: snapshot.ObservedAt}
			if w.ResetsAt != nil {
				b.Until = *w.ResetsAt
			} else if w.Name == "provider" {
				// This is a provider gate with an unknown reset, not an
				// entitlement to invent a five-hour/weekly deadline.
				b.Until = snapshot.ObservedAt.Add(time.Minute)
			}
			// Replace matching telemetry blockers instead of accumulating a new
			// copy on every poll. Rejection timestamps are never moved backward.
			found := false
			for i, old := range blocks {
				if old.Kind == b.Kind && old.Source == b.Source && old.Model == b.Model && old.Window == b.Window {
					if !old.CreatedAt.After(b.CreatedAt) {
						blocks[i] = b
					}
					found = true
					break
				}
			}
			if !found {
				blocks = append(blocks, b)
			}
		}
	}
	if snapshot.AuthInvalid {
		found := false
		for _, b := range blocks {
			if b.Kind == "auth" {
				found = true
				break
			}
		}
		if !found {
			blocks = append(blocks, blocker{Kind: "auth", Source: "telemetry", CreatedAt: snapshot.ObservedAt})
		}
	}
	// Keep independently reported windows absent from a partial read. Their
	// original observation clocks remain separate, so they do not become fresh.
	// A complete, available read is authoritative: a missing window was retired,
	// so its reading, first-use clock and learned scale are dropped too.
	retire := snapshot.Complete && snapshot.Available
	for _, w := range oldSnapshot.Windows {
		k := windowKey(w)
		if present[k] {
			continue
		}
		if retire {
			delete(observed, k)
			delete(clocks, k)
			delete(learned, k)
			continue
		}
		snapshot.Windows = append(snapshot.Windows, w)
	}
	s.state.Snapshots[id] = cloneSnapshot(snapshot)
	s.state.Learning[id], s.state.Clocks[id], s.state.Blockers[id] = learned, clocks, blocks
	s.state.WindowObserved[id] = observed
	if err := s.save(); err != nil {
		if existed {
			s.state.Snapshots[id] = oldSnapshot
		} else {
			delete(s.state.Snapshots, id)
		}
		s.state.Learning[id], s.state.Clocks[id], s.state.Blockers[id] = oldLearning, oldClocks, oldBlocks
		s.state.WindowObserved[id] = oldObserved
		return err
	}
	return nil
}

func (s *Scheduler) snapshotClears(snapshot model.Snapshot, b blocker) bool {
	if !snapshot.ObservedAt.After(b.CreatedAt) {
		return false
	}
	if b.Kind == "auth" {
		return snapshot.Available && !snapshot.AuthInvalid
	}
	if b.Kind != "quota" || !snapshot.Available {
		return false
	}
	matched := false
	for _, w := range snapshot.Windows {
		if !applicable(w.Model, b.Model) || !sameWindow(b.Window, w) {
			continue
		}
		// Generic account telemetry cannot prove recovery from a model-specific
		// backend rejection. Rounded percentages cannot override the backend.
		if b.Model != "" && w.Model == "" {
			continue
		}
		// A global rejection needs positive telemetry for global windows; a
		// model-only bucket cannot prove the whole account is usable.
		if b.Model == "" && w.Model != "" {
			continue
		}
		if b.Source == "backend" && (w.Allowed == nil || !*w.Allowed) {
			return false
		}
		if windowExhausted(w) || (w.Used == nil && (w.Allowed == nil || !*w.Allowed)) {
			return false
		}
		if w.ResetsAt != nil && !w.ResetsAt.After(snapshot.ObservedAt) {
			return false
		}
		matched = true
	}
	return matched
}

// eligibility combines every applicable blocker. For affinity preservation,
// ignoreTransient disregards transport and throughput failures.
func (s *Scheduler) eligibility(id, modelName string, now time.Time, ignoreTransient bool) (usable, known bool, retry time.Time) {
	usable, known = true, true
	indefinite := false
	block := func(until time.Time) {
		usable = false
		if until.IsZero() {
			indefinite = true
		} else if until.After(retry) {
			retry = until
		}
	}
	for _, b := range s.state.Blockers[id] {
		if !applicable(b.Model, modelName) {
			continue
		}
		if ignoreTransient && (b.Kind == "transient" || b.Kind == "throughput") {
			continue
		}
		if !b.Until.IsZero() && !b.Until.After(now) {
			continue
		}
		block(b.Until)
	}
	snapshot, ok := s.state.Snapshots[id]
	fresh := ok && snapshot.Available && !snapshot.AuthInvalid && now.Sub(snapshot.ObservedAt) <= telemetryAge
	if snapshot.AuthInvalid {
		block(time.Time{})
	}
	if !fresh {
		known = false
	}
	applicableWindows := 0
	for _, w := range snapshot.Windows {
		if !applicable(w.Model, modelName) {
			continue
		}
		applicableWindows++
		if observed := s.state.WindowObserved[id][windowKey(w)]; observed.IsZero() || now.Sub(observed) > telemetryAge {
			known = false
		}
		// A past reset means this observation belongs to an old window. It
		// cannot prove exhaustion or fresh availability in the new window.
		if w.ResetsAt != nil && !w.ResetsAt.After(now) {
			known = false
			continue
		}
		if windowExhausted(w) {
			until := time.Time{}
			if w.ResetsAt != nil {
				until = *w.ResetsAt
			} else if w.Name == "provider" {
				until = s.state.WindowObserved[id][windowKey(w)].Add(time.Minute)
				if !until.After(now) {
					known = false
					continue
				}
			}
			block(until)
		}
		if w.Used == nil && (w.Allowed == nil || !*w.Allowed) {
			known = false
		}
	}
	if applicableWindows == 0 {
		known = false
	}
	if indefinite {
		retry = time.Time{}
	}
	return
}

func windowExhausted(w model.Window) bool {
	if w.Exhausted {
		return true
	}
	if w.Allowed != nil {
		return !*w.Allowed
	}
	return w.Used != nil && *w.Used >= 100
}

// score estimates useful work in each window's own learned units. The bounded
// six-hour horizon gives expiring allowances priority, then load discounts
// spread concurrent agents. Percentages from 5h and week are never compared as
// interchangeable work units. The minute/request prior is replaced by measured
// per-window consumption as soon as real work and telemetry provide a sample.
func (s *Scheduler) score(id, modelName string, now time.Time) float64 {
	const horizon = 6 * time.Hour
	const expectedWork = time.Minute
	capacity := math.Inf(1)
	expiry := 0.0
	load := s.inflight(id, "")
	snapshot := s.state.Snapshots[id]
	for _, w := range snapshot.Windows {
		if !applicable(w.Model, modelName) || w.Used == nil || (w.ResetsAt != nil && !w.ResetsAt.After(now)) {
			continue
		}
		capacity = math.Min(capacity, s.windowWork(id, w))
	}
	if math.IsInf(capacity, 1) {
		capacity = 1
	}
	for _, w := range snapshot.Windows {
		if !applicable(w.Model, modelName) || w.Used == nil {
			continue
		}
		if w.ResetsAt != nil && !w.ResetsAt.After(now) {
			continue
		}
		work := s.windowWork(id, w)
		reset := s.state.Clocks[id][windowKey(w)]
		if w.ResetsAt != nil {
			reset = *w.ResetsAt
		}
		remaining := reset.Sub(now)
		if remaining > 0 && remaining <= horizon {
			// Cap the lookahead at a plausible amount of sequential useful
			// work. Capacity beyond that horizon is not treated as reachable.
			// Another window can prevent this allowance from being spent. Do
			// not value unreachable quota as though it were immediately usable.
			reachable := math.Min(work, capacity)
			// Existing sticky sessions also consume work between HTTP calls.
			// Reserve their forecast demand before assigning more new sessions.
			_, rate := s.recentDemand(id, w.Model, now)
			unassigned := math.Max(0, reachable-rate*remaining.Seconds())
			expiry += math.Min(unassigned, float64(horizon/expectedWork)) / (remaining.Seconds() + expectedWork.Seconds())
		}
	}
	active, _ := s.recentDemand(id, "", now)
	// Even a tiny positive allowance remains eligible. Reservations influence
	// placement, never invent a hard percentage reserve or evict a binding.
	return (1 + 100*expiry + math.Log1p(capacity)/10) / (math.Pow(float64(load+1), 2) * math.Sqrt(float64(active+1)))
}

// recentDemand is only a forecast for NEW placement. It never expires a binding
// or limits an already-bound session. Long-idle sessions stop reserving demand.
func (s *Scheduler) recentDemand(id, scope string, now time.Time) (int, float64) {
	count, rate := 0, 0.0
	for key, activity := range s.state.Activity {
		if activity.AccountID != id || s.state.Bindings[key] != id || !applicable(scope, activity.Model) {
			continue
		}
		cadence := activity.CadenceSeconds
		if math.IsNaN(cadence) || math.IsInf(cadence, 0) || cadence <= 0 {
			cadence = 60
		}
		cadence = math.Max(1, math.Min(cadence, 1800))
		freshFor := math.Min(1800, math.Max(300, 3*cadence))
		age := now.Sub(activity.LastRequest).Seconds()
		if age < 0 || age > freshFor {
			continue
		}
		count++
		rate += 1 / cadence
	}
	return count, rate
}

func (s *Scheduler) windowWork(id string, w model.Window) float64 {
	l := s.state.Learning[id][windowKey(w)]
	mean := l.Mean
	if mean <= 0 {
		if w.Duration > 0 {
			mean = 100 * float64(time.Minute) / float64(w.Duration)
		} else {
			mean = 1
		}
	}
	count := s.inflight(id, w.Model)
	// Completion transfers the estimate from in-flight to pending telemetry.
	// A subsequent read advances the baseline, replacing those estimates with
	// actual utilization instead of charging the same completed request twice.
	pending := uint64(0)
	if completed := s.completedFor(id, w.Model); completed > l.Completed {
		pending = completed - l.Completed
	}
	reserved := (float64(count) + float64(pending)) * mean
	return math.Max(0, 100-*w.Used-reserved) / mean
}

type accountStatus struct {
	Email           string               `json:"email,omitempty"`
	Alias           string               `json:"alias,omitempty"`
	DisplayName     string               `json:"display_name"`
	ID              string               `json:"id"`
	Label           string               `json:"label"`
	Provider        string               `json:"provider"`
	Enabled         bool                 `json:"enabled"`
	Inflight        int                  `json:"inflight"`
	RecentSessions  int                  `json:"recent_sessions"`
	Snapshot        *model.Snapshot      `json:"snapshot,omitempty"`
	Blockers        []blocker            `json:"blockers,omitempty"`
	PredictedResets map[string]time.Time `json:"predicted_resets,omitempty"`
	WindowObserved  map[string]time.Time `json:"window_observed,omitempty"`
}

// Status omits profile paths, endpoints, session identifiers and all secrets.
func (s *Scheduler) Status() any {
	s.mu.Lock()
	defer s.mu.Unlock()
	result := struct {
		Bindings int             `json:"bindings"`
		Accounts []accountStatus `json:"accounts"`
	}{Bindings: len(s.state.Bindings), Accounts: make([]accountStatus, 0, len(s.accounts))}
	for _, a := range s.accounts {
		item := accountStatus{ID: a.ID, Email: a.Email, Alias: a.Alias, DisplayName: a.DisplayName(), Label: a.Label(), Provider: a.Provider, Enabled: a.Enabled, Inflight: s.inflight(a.ID, "")}
		item.RecentSessions, _ = s.recentDemand(a.ID, "", s.now())
		if snapshot, ok := s.state.Snapshots[a.ID]; ok {
			snapshot = cloneSnapshot(snapshot)
			item.Snapshot = &snapshot
		}
		for _, b := range s.state.Blockers[a.ID] {
			if b.Until.IsZero() || b.Until.After(s.now()) {
				item.Blockers = append(item.Blockers, b)
			}
		}
		if clocks := s.state.Clocks[a.ID]; len(clocks) > 0 {
			item.PredictedResets = map[string]time.Time{}
			for k, v := range clocks {
				item.PredictedResets[k] = v
			}
		}
		if observed := s.state.WindowObserved[a.ID]; len(observed) > 0 {
			item.WindowObserved = map[string]time.Time{}
			for k, v := range observed {
				item.WindowObserved[k] = v
			}
		}
		result.Accounts = append(result.Accounts, item)
	}
	return result
}
