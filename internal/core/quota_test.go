package core

import (
	"math"
	"strings"
	"testing"
	"time"

	"github.com/amxv/noria/internal/model"
)

func TestAllWindowsGateAndForecastRequiresEveryBlocker(t *testing.T) {
	s, now := testScheduler(t, "a", "b", "c")
	update(t, s, "a", *now, window("short", 100, now.Add(5*time.Minute), 5*time.Hour), window("week", 100, now.Add(time.Hour), 7*24*time.Hour))
	update(t, s, "b", *now, window("short", 100, now.Add(30*time.Minute), 5*time.Hour))
	if err := s.Update(model.Snapshot{AccountID: "b", ObservedAt: now.Add(time.Nanosecond), AuthInvalid: true}); err != nil {
		t.Fatal(err)
	}
	update(t, s, "c", *now, window("short", 100, now.Add(40*time.Minute), 5*time.Hour))
	err := unavailable(t, s, "gpt-5.3-codex")
	if !err.RetryAt.Equal(now.Add(40 * time.Minute)) {
		t.Fatalf("forecast ignored later window or auth blocker: %s", err.RetryAt)
	}
	*now = now.Add(6 * time.Minute)
	err = unavailable(t, s, "gpt-5.3-codex")
	if !err.RetryAt.Equal(now.Add(34 * time.Minute)) {
		t.Fatalf("short reset cleared weekly blocker: %s", err.RetryAt)
	}
}

func TestModelScopedWindows(t *testing.T) {
	s, now := testScheduler(t, "a")
	scoped := window("seven_day_sonnet", 100, now.Add(time.Hour), 7*24*time.Hour)
	scoped.Model = "sonnet"
	update(t, s, "a", *now, window("five_hour", 50, now.Add(time.Hour), 5*time.Hour), scoped)
	unavailable(t, s, "claude-sonnet-4-6")
	l, err := s.Acquire("codex", "", "claude-opus-4-6", nil)
	if err != nil {
		t.Fatal("unrelated model blocked by sonnet limit:", err)
	}
	_ = s.Complete(l)
	*now = now.Add(time.Second)
	scoped.Model = "GPT-5.3-Codex"
	update(t, s, "a", *now, scoped)
	unavailable(t, s, "gpt-5.3-codex")
	l, err = s.Acquire("codex", "", "gpt-5.3-codex-spark", nil)
	if err != nil {
		t.Fatal("scoped exact bucket leaked to unrelated model:", err)
	}
	_ = s.Complete(l)
	if _, err = s.Acquire("claude", "", "allowed-model", nil); err == nil {
		t.Fatal("cross-provider account selected")
	}
}

func TestQuotaRejectionWinsOverStaleReadsAndNullTelemetry(t *testing.T) {
	s, now := testScheduler(t, "a")
	update(t, s, "a", now.Add(-time.Minute), window("short", 10, now.Add(time.Hour), 5*time.Hour))
	l := acquire(t, s, "session")
	if err := s.Reject(l, model.Rejection{Kind: "quota", Window: "short", Until: now.Add(time.Hour)}); err != nil {
		t.Fatal(err)
	}
	update(t, s, "a", now.Add(-time.Second), window("short", 11, now.Add(time.Hour), 5*time.Hour))
	unavailable(t, s, "gpt-5.3-codex")
	*now = now.Add(time.Second)
	update(t, s, "a", *now, model.Window{Name: "short", Duration: 5 * time.Hour})
	unavailable(t, s, "gpt-5.3-codex")
	*now = now.Add(time.Second)
	update(t, s, "a", *now, window("different", 5, now.Add(time.Hour), 5*time.Hour))
	unavailable(t, s, "gpt-5.3-codex")
	*now = now.Add(time.Second)
	update(t, s, "a", *now, window("short", 12, now.Add(time.Hour), 5*time.Hour))
	unavailable(t, s, "gpt-5.3-codex")
	*now = now.Add(time.Second)
	recovered := window("short", 12, now.Add(time.Hour), 5*time.Hour)
	allowed := true
	recovered.Allowed = &allowed
	update(t, s, "a", *now, recovered)
	l = acquire(t, s, "session")
	_ = s.Complete(l)
}

func TestBackendQuotaBeatsFreshDisplayPercentAndUnknownResetProbesAreBounded(t *testing.T) {
	s, now := testScheduler(t, "a")
	update(t, s, "a", *now, window("short", 95, now.Add(time.Hour), 5*time.Hour), window("week", 20, now.Add(7*24*time.Hour), 7*24*time.Hour))
	l := acquire(t, s, "")
	if err := s.Reject(l, model.Rejection{Kind: "quota"}); err != nil {
		t.Fatal(err)
	}
	*now = now.Add(time.Second)
	update(t, s, "a", *now, window("short", 95, now.Add(time.Hour), 5*time.Hour), window("week", 20, now.Add(7*24*time.Hour), 7*24*time.Hour))
	err := unavailable(t, s, "gpt-5.3-codex")
	if !err.RetryAt.Equal(now.Add(59 * time.Second)) {
		t.Fatalf("unknown quota reset used unrelated week or allowed immediate probe: %s", err.RetryAt)
	}
	*now = now.Add(time.Minute)
	_ = s.Complete(acquire(t, s, ""))
}

func TestBackendQuotaUsesOnlyExhaustedWindowReset(t *testing.T) {
	s, now := testScheduler(t, "a")
	shortReset := now.Add(time.Hour)
	weekReset := now.Add(7 * 24 * time.Hour)
	update(t, s, "a", *now, window("short", 95, shortReset, 5*time.Hour), window("week", 20, weekReset, 7*24*time.Hour))
	l := acquire(t, s, "")
	*now = now.Add(time.Second)
	update(t, s, "a", *now, window("short", 100, shortReset, 5*time.Hour), window("week", 20, weekReset, 7*24*time.Hour))
	if err := s.Reject(l, model.Rejection{Kind: "quota"}); err != nil {
		t.Fatal(err)
	}
	err := unavailable(t, s, "gpt-5.3-codex")
	if !err.RetryAt.Equal(shortReset) {
		t.Fatalf("healthy week extended actual five-hour rejection: %s", err.RetryAt)
	}
}

func TestModelBackendRejectionNeedsMatchingScopedAuthority(t *testing.T) {
	s, now := testScheduler(t, "a")
	update(t, s, "a", *now, window("short", 20, now.Add(time.Hour), 5*time.Hour))
	l, err := s.Acquire("codex", "", "claude-sonnet-4-6", nil)
	if err != nil {
		t.Fatal(err)
	}
	if err := s.Reject(l, model.Rejection{Kind: "quota", Model: "claude-sonnet-4-6", Until: now.Add(time.Hour)}); err != nil {
		t.Fatal(err)
	}
	*now = now.Add(time.Second)
	allowed := true
	global := window("short", 20, now.Add(time.Hour), 5*time.Hour)
	global.Allowed = &allowed
	update(t, s, "a", *now, global)
	unavailable(t, s, "claude-sonnet-4-6")
	l, err = s.Acquire("codex", "", "claude-opus-4-6", nil)
	if err != nil {
		t.Fatal("scoped backend rejection blocked unrelated family:", err)
	}
	_ = s.Complete(l)
	*now = now.Add(time.Second)
	scoped := window("week-sonnet", 50, now.Add(time.Hour), 7*24*time.Hour)
	scoped.Model = "sonnet"
	scoped.Allowed = &allowed
	update(t, s, "a", *now, global, scoped)
	l, err = s.Acquire("codex", "", "claude-sonnet-4-6", nil)
	if err != nil {
		t.Fatal("matching scoped authority did not clear rejection:", err)
	}
	_ = s.Complete(l)
}

func TestPartialAndUnavailableSnapshotsDoNotEraseExhaustion(t *testing.T) {
	s, now := testScheduler(t, "a")
	update(t, s, "a", *now, window("short", 10, now.Add(time.Hour), 5*time.Hour), window("week", 100, now.Add(24*time.Hour), 7*24*time.Hour))
	*now = now.Add(time.Second)
	update(t, s, "a", *now, window("short", 11, now.Add(time.Hour), 5*time.Hour))
	unavailable(t, s, "gpt-5.3-codex")
	*now = now.Add(time.Second)
	if err := s.Update(model.Snapshot{AccountID: "a", ObservedAt: *now, Available: false}); err != nil {
		t.Fatal(err)
	}
	unavailable(t, s, "gpt-5.3-codex")
	*now = now.Add(time.Second)
	update(t, s, "a", *now, window("week", 50, now.Add(24*time.Hour), 7*24*time.Hour))
	_ = s.Complete(acquire(t, s, ""))
}

func TestAuthoritativeAllowedOverridesRoundedPercentage(t *testing.T) {
	s, now := testScheduler(t, "a")
	allowed := true
	w := window("short", 100, now.Add(time.Hour), 5*time.Hour)
	w.Allowed = &allowed
	update(t, s, "a", *now, w)
	_ = s.Complete(acquire(t, s, ""))
	*now = now.Add(time.Second)
	allowed = false
	w = window("short", 10, now.Add(time.Hour), 5*time.Hour)
	w.Allowed = &allowed
	update(t, s, "a", *now, w)
	unavailable(t, s, "gpt-5.3-codex")
	*now = now.Add(time.Second)
	allowed = true
	w = window("short", 100, now.Add(time.Hour), 5*time.Hour)
	w.Allowed = &allowed
	update(t, s, "a", *now, w)
	_ = s.Complete(acquire(t, s, ""))
	*now = now.Add(time.Second)
	w.Exhausted = true
	update(t, s, "a", *now, w)
	unavailable(t, s, "gpt-5.3-codex")
}

func TestFirstUseClocksStartOnlyOnAcceptedRealWorkAndYieldToAuthority(t *testing.T) {
	s, now := testScheduler(t, "a")
	short := window("short", 0, time.Time{}, 5*time.Hour)
	week := window("week", 0, time.Time{}, 7*24*time.Hour)
	scoped := window("seven_day_sonnet", 0, time.Time{}, 7*24*time.Hour)
	scoped.Model = "sonnet"
	short.Dormant, week.Dormant, scoped.Dormant = true, true, true
	update(t, s, "a", *now, short, week, scoped)
	if len(s.state.Clocks["a"]) != 0 {
		t.Fatal("telemetry started an unstarted window")
	}
	l := acquire(t, s, "session")
	if len(s.state.Clocks["a"]) != 0 {
		t.Fatal("reservation started an unstarted window")
	}
	_ = s.Complete(l)
	if len(s.state.Clocks["a"]) != 0 {
		t.Fatal("cancelled reservation started an unstarted window")
	}
	l = acquire(t, s, "session")
	if err := s.Accept(l); err != nil {
		t.Fatal(err)
	}
	if got := s.state.Clocks["a"][windowKey(short)]; !got.Equal(now.Add(5 * time.Hour)) {
		t.Fatalf("wrong first-use short clock %s", got)
	}
	if got := s.state.Clocks["a"][windowKey(week)]; !got.Equal(now.Add(7 * 24 * time.Hour)) {
		t.Fatalf("wrong first-use weekly clock %s", got)
	}
	if !s.state.Clocks["a"][windowKey(scoped)].IsZero() {
		t.Fatal("unrelated model started a scoped first-use clock")
	}
	_ = s.Complete(l)
	*now = now.Add(time.Minute)
	reset := now.Add(2 * time.Hour)
	short.ResetsAt = &reset
	update(t, s, "a", *now, short)
	acceptComplete(t, s, acquire(t, s, "session"))
	if !s.state.Clocks["a"][windowKey(short)].IsZero() {
		t.Fatal("prediction overrode authoritative reset")
	}
	if got := s.state.Snapshots["a"].Windows[0].ResetsAt; got == nil || !got.Equal(reset) {
		t.Fatal("authoritative reset was lost")
	}
}

func TestLearnWindowSpecificConsumptionAndReconcileReservations(t *testing.T) {
	s, now := testScheduler(t, "a")
	short := window("short", 10, now.Add(5*time.Hour), 5*time.Hour)
	week := window("week", 10, now.Add(7*24*time.Hour), 7*24*time.Hour)
	update(t, s, "a", *now, short, week)
	acceptComplete(t, s, acquire(t, s, ""))
	acceptComplete(t, s, acquire(t, s, ""))
	*now = now.Add(time.Second)
	short = window("short", 14, *short.ResetsAt, 5*time.Hour)
	week = window("week", 10.2, *week.ResetsAt, 7*24*time.Hour)
	update(t, s, "a", *now, short, week)
	if got := s.state.Learning["a"][windowKey(short)].Mean; math.Abs(got-2) > 1e-9 {
		t.Fatalf("short normalization %.10f", got)
	}
	if got := s.state.Learning["a"][windowKey(week)].Mean; math.Abs(got-.1) > 1e-9 {
		t.Fatalf("week normalization %.10f", got)
	}
	l := acquire(t, s, "")
	reserved := s.windowWork("a", short)
	acceptComplete(t, s, l)
	if got := s.windowWork("a", short); got != reserved {
		t.Fatalf("completion lost or doubled reservation: %.4f vs %.4f", reserved, got)
	}
	*now = now.Add(time.Second)
	short = window("short", 16, *short.ResetsAt, 5*time.Hour)
	update(t, s, "a", *now, short)
	if got := s.windowWork("a", short); got != 42 {
		t.Fatalf("telemetry double-counted completed work: %.4f", got)
	}
	// Reset decreases are not learned as negative consumption.
	*now = now.Add(time.Second)
	short = window("short", 0, now.Add(5*time.Hour), 5*time.Hour)
	update(t, s, "a", *now, short)
	if got := s.state.Learning["a"][windowKey(short)].Mean; got != 2 {
		t.Fatalf("reset corrupted learned cost: %.4f", got)
	}
}

func TestFreshReadAndInputCopies(t *testing.T) {
	s, now := testScheduler(t, "a", "b")
	w := window("short", 20, now.Add(time.Hour), 5*time.Hour)
	update(t, s, "a", *now, w)
	*w.Used = 100
	*w.ResetsAt = now.Add(3 * time.Hour)
	if l := acquire(t, s, ""); l.Account.ID != "a" {
		t.Fatal("caller mutated scheduler snapshot")
	} else {
		_ = s.Complete(l)
	}
	*now = now.Add(3 * time.Minute)
	update(t, s, "b", *now, window("short", 90, now.Add(time.Hour), 5*time.Hour))
	if l := acquire(t, s, ""); l.Account.ID != "b" {
		t.Fatal("stale observation displaced fresh telemetry")
	} else {
		_ = s.Complete(l)
	}
}

func TestScopedAccountingDoesNotChargeOtherModelFamilies(t *testing.T) {
	s, now := testScheduler(t, "a")
	w := window("seven_day_sonnet", 20, now.Add(24*time.Hour), 7*24*time.Hour)
	w.Model = "sonnet"
	update(t, s, "a", *now, w)
	before := s.windowWork("a", w)
	l, err := s.Acquire("codex", "", "claude-opus-4-6", nil)
	if err != nil {
		t.Fatal(err)
	}
	if got := s.windowWork("a", w); got != before {
		t.Fatal("opus reservation charged sonnet window")
	}
	acceptComplete(t, s, l)
	if got := s.windowWork("a", w); got != before {
		t.Fatal("opus completion charged sonnet window")
	}
	l, err = s.Acquire("codex", "", "claude-sonnet-4-5", nil)
	if err != nil {
		t.Fatal(err)
	}
	if got := s.windowWork("a", w); got >= before {
		t.Fatal("sonnet reservation failed to charge family window")
	}
	acceptComplete(t, s, l)
	*now = now.Add(time.Second)
	w = window("seven_day_sonnet", 22, *w.ResetsAt, 7*24*time.Hour)
	w.Model = "sonnet"
	update(t, s, "a", *now, w)
	if mean := s.state.Learning["a"][windowKey(w)].Mean; mean != 2 {
		t.Fatalf("unrelated model diluted learned scope cost: %f", mean)
	}
}

func TestCompleteSnapshotRetiresWindowsTheProviderNoLongerReports(t *testing.T) {
	s, now := testScheduler(t, "a")
	update(t, s, "a", *now, window("five_hour", 16, now.Add(time.Hour), 5*time.Hour), window("seven_day", 70, now.Add(24*time.Hour), 7*24*time.Hour))
	*now = now.Add(time.Second)
	// A partial read keeps the unreported window.
	update(t, s, "a", *now, window("seven_day", 72, now.Add(24*time.Hour), 7*24*time.Hour))
	if got := len(s.state.Snapshots["a"].Windows); got != 2 {
		t.Fatalf("partial read dropped a window: %d", got)
	}
	*now = now.Add(time.Second)
	if err := s.Update(model.Snapshot{AccountID: "a", ObservedAt: *now, Available: true, Complete: true,
		Windows: []model.Window{window("seven_day", 75, now.Add(24*time.Hour), 7*24*time.Hour)}}); err != nil {
		t.Fatal(err)
	}
	windows := s.state.Snapshots["a"].Windows
	if len(windows) != 1 || windows[0].Name != "seven_day" {
		t.Fatalf("complete read kept a retired window: %+v", windows)
	}
	for k := range s.state.WindowObserved["a"] {
		if strings.Contains(k, "five_hour") {
			t.Fatal("retired window observation clock kept")
		}
	}
}
