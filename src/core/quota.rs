use super::scheduler::{applicable, restore, same_window, seconds, window_key};
use super::*;
use crate::model::Window;
use std::collections::HashSet;

impl Scheduler {
    /// A read's start timestamp prevents stale reads from clearing newer refusals.
    pub fn update(&self, mut snapshot: Snapshot) -> Result<()> {
        let mut s = self.guard();
        let id = snapshot.account_id.clone();
        if id.is_empty() {
            bail!("snapshot account ID is required");
        }
        if snapshot.observed_at == model::zero_time() {
            bail!("snapshot observation time is required");
        }
        if snapshot.observed_at > (self.now)() + Duration::minutes(1) {
            bail!("snapshot observation time is in the future");
        }
        for w in &snapshot.windows {
            if w.duration < 0 {
                bail!("invalid negative quota duration");
            }
            if w.used
                .is_some_and(|v| !v.is_finite() || !(0.0..=100.0).contains(&v))
            {
                bail!("quota utilization outside 0..100");
            }
        }
        let old_snapshot = s.state.snapshots.get(&id).cloned();
        if old_snapshot
            .as_ref()
            .is_some_and(|old| snapshot.observed_at <= old.observed_at)
        {
            return Ok(());
        }
        let old_blocks = s.state.blockers.get(&id).cloned();
        let old_learning = s.state.learning.get(&id).cloned();
        let old_clocks = s.state.clocks.get(&id).cloned();
        let old_observed = s.state.window_observed.get(&id).cloned();
        let mut learned = old_learning.clone().unwrap_or_default();
        let mut clocks = old_clocks.clone().unwrap_or_default();
        let mut observed = old_observed.clone().unwrap_or_default();
        let mut present = HashSet::new();
        for w in &snapshot.windows {
            let key = window_key(w);
            present.insert(key.clone());
            observed.insert(key.clone(), snapshot.observed_at);
            if w.resets_at.is_some() {
                clocks.remove(&key);
            }
            let Some(used) = w.used else {
                continue;
            };
            let completed = s.completed_for(&id, &w.model);
            let had_learning = learned.contains_key(&key);
            let l = learned.entry(key).or_default();
            if had_learning && l.reset_at == w.resets_at && completed > l.completed && used > l.used
            {
                let delta = (used - l.used) / (completed - l.completed) as f64;
                l.mean = if l.samples == 0 {
                    delta
                } else {
                    0.7 * l.mean + 0.3 * delta
                };
                l.samples += 1;
            }
            l.used = used;
            l.completed = completed;
            l.observed_at = snapshot.observed_at;
            l.reset_at = w.resets_at;
        }
        let mut blocks: Vec<Blocker> = old_blocks
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|b| !snapshot_clears(&snapshot, b))
            .collect();
        for w in &snapshot.windows {
            if !window_exhausted(w) {
                continue;
            }
            let b = Blocker {
                kind: "quota".into(),
                source: "telemetry".into(),
                model: w.model.clone(),
                window: w.name.clone(),
                created_at: snapshot.observed_at,
                until: w.resets_at.or_else(|| {
                    (w.name == "provider").then_some(snapshot.observed_at + Duration::minutes(1))
                }),
            };
            if let Some(old) = blocks.iter_mut().find(|old| {
                old.kind == b.kind
                    && old.source == b.source
                    && old.model == b.model
                    && old.window == b.window
            }) {
                if old.created_at <= b.created_at {
                    *old = b;
                }
            } else {
                blocks.push(b);
            }
        }
        if snapshot.auth_invalid && !blocks.iter().any(|b| b.kind == "auth") {
            blocks.push(Blocker {
                kind: "auth".into(),
                source: "telemetry".into(),
                created_at: snapshot.observed_at,
                ..Blocker::default()
            });
        }
        if let Some(old) = &old_snapshot {
            for w in &old.windows {
                if !present.contains(&window_key(w)) {
                    snapshot.windows.push(w.clone());
                }
            }
        }
        s.state.snapshots.insert(id.clone(), snapshot);
        s.state.learning.insert(id.clone(), learned);
        s.state.clocks.insert(id.clone(), clocks);
        s.state.blockers.insert(id.clone(), blocks);
        s.state.window_observed.insert(id.clone(), observed);
        if let Err(e) = s.save() {
            restore(&mut s.state.snapshots, &id, old_snapshot);
            restore(&mut s.state.learning, &id, old_learning);
            restore(&mut s.state.clocks, &id, old_clocks);
            restore(&mut s.state.blockers, &id, old_blocks);
            restore(&mut s.state.window_observed, &id, old_observed);
            return Err(e);
        }
        Ok(())
    }

    /// Omit paths, endpoints, session identifiers, and credentials.
    pub fn status(&self) -> Status {
        let s = self.guard();
        let now = (self.now)();
        Status {
            bindings: s.state.bindings.len(),
            accounts: s
                .accounts
                .iter()
                .map(|a| AccountStatus {
                    id: a.id.clone(),
                    email: a.email.clone(),
                    alias: a.alias.clone(),
                    display_name: a.display_name().into(),
                    label: a.label().into(),
                    provider: a.provider.clone(),
                    enabled: a.enabled,
                    inflight: s.inflight(&a.id, ""),
                    recent_sessions: s.recent_demand(&a.id, "", now).0,
                    snapshot: s.state.snapshots.get(&a.id).cloned(),
                    blockers: s
                        .state
                        .blockers
                        .get(&a.id)
                        .into_iter()
                        .flatten()
                        .filter(|b| b.until.is_none_or(|at| at > now))
                        .cloned()
                        .collect(),
                    predicted_resets: s.state.clocks.get(&a.id).cloned().unwrap_or_default(),
                    window_observed: s
                        .state
                        .window_observed
                        .get(&a.id)
                        .cloned()
                        .unwrap_or_default(),
                })
                .collect(),
        }
    }
}

fn snapshot_clears(snapshot: &Snapshot, b: &Blocker) -> bool {
    if snapshot.observed_at <= b.created_at {
        return false;
    }
    if b.kind == "auth" {
        return snapshot.available && !snapshot.auth_invalid;
    }
    if b.kind != "quota" || !snapshot.available {
        return false;
    }
    let mut matched = false;
    for w in &snapshot.windows {
        if !applicable(&w.model, &b.model) || !same_window(&b.window, w) {
            continue;
        }
        if !b.model.is_empty() && w.model.is_empty() || b.model.is_empty() && !w.model.is_empty() {
            continue;
        }
        if b.source == "backend" && w.allowed != Some(true) {
            return false;
        }
        if window_exhausted(w) || w.used.is_none() && w.allowed != Some(true) {
            return false;
        }
        if w.resets_at.is_some_and(|at| at <= snapshot.observed_at) {
            return false;
        }
        matched = true;
    }
    matched
}
pub(super) fn window_exhausted(w: &Window) -> bool {
    w.exhausted
        || w.allowed
            .map(|a| !a)
            .unwrap_or_else(|| w.used.is_some_and(|v| v >= 100.0))
}

impl Inner {
    pub(super) fn eligibility(
        &self,
        id: &str,
        model: &str,
        now: DateTime<Utc>,
        ignore_transient: bool,
    ) -> (bool, bool, Option<DateTime<Utc>>) {
        let mut usable = true;
        let mut known = true;
        let mut indefinite = false;
        let mut retry = None;
        let mut block = |until: Option<DateTime<Utc>>| {
            usable = false;
            if let Some(at) = until {
                if retry.is_none_or(|old| at > old) {
                    retry = Some(at);
                }
            } else {
                indefinite = true;
            }
        };
        for b in self.state.blockers.get(id).into_iter().flatten() {
            if !applicable(&b.model, model)
                || ignore_transient && (b.kind == "transient" || b.kind == "throughput")
                || b.until.is_some_and(|at| at <= now)
            {
                continue;
            }
            block(b.until);
        }
        let snapshot = self.state.snapshots.get(id);
        let fresh = snapshot.is_some_and(|s| {
            s.available && !s.auth_invalid && now - s.observed_at <= TELEMETRY_AGE
        });
        if snapshot.is_some_and(|s| s.auth_invalid) {
            block(None);
        }
        if !fresh {
            known = false;
        }
        let mut applicable_windows = 0;
        for w in snapshot.into_iter().flat_map(|s| &s.windows) {
            if !applicable(&w.model, model) {
                continue;
            }
            applicable_windows += 1;
            let observed = self
                .state
                .window_observed
                .get(id)
                .and_then(|m| m.get(&window_key(w)))
                .copied();
            if observed.is_none_or(|at| now - at > TELEMETRY_AGE) {
                known = false;
            }
            if w.resets_at.is_some_and(|at| at <= now) {
                known = false;
                continue;
            }
            if window_exhausted(w) {
                let until = w.resets_at.or_else(|| {
                    (w.name == "provider")
                        .then(|| observed.unwrap_or_else(model::zero_time) + Duration::minutes(1))
                });
                if w.resets_at.is_none()
                    && w.name == "provider"
                    && until.is_some_and(|at| at <= now)
                {
                    known = false;
                    continue;
                }
                block(until);
            }
            if w.used.is_none() && w.allowed != Some(true) {
                known = false;
            }
        }
        if applicable_windows == 0 {
            known = false;
        }
        (usable, known, if indefinite { None } else { retry })
    }

    pub(super) fn score(&self, id: &str, model: &str, now: DateTime<Utc>) -> f64 {
        let horizon = 6.0 * 3600.0;
        let mut capacity = f64::INFINITY;
        let mut expiry = 0.0;
        let windows = self
            .state
            .snapshots
            .get(id)
            .map(|s| s.windows.as_slice())
            .unwrap_or(&[]);
        for w in windows {
            if applicable(&w.model, model)
                && w.used.is_some()
                && w.resets_at.is_none_or(|at| at > now)
            {
                capacity = capacity.min(self.window_work(id, w));
            }
        }
        if capacity == f64::INFINITY {
            capacity = 1.0;
        }
        for w in windows {
            if !applicable(&w.model, model)
                || w.used.is_none()
                || w.resets_at.is_some_and(|at| at <= now)
            {
                continue;
            }
            let reset = w.resets_at.or_else(|| {
                self.state
                    .clocks
                    .get(id)
                    .and_then(|m| m.get(&window_key(w)))
                    .copied()
            });
            let Some(reset) = reset else {
                continue;
            };
            let remaining = seconds(reset - now);
            if remaining > 0.0 && remaining <= horizon {
                let reachable = self.window_work(id, w).min(capacity);
                let (_, rate) = self.recent_demand(id, &w.model, now);
                let unassigned = (reachable - rate * remaining).max(0.0);
                expiry += unassigned.min(horizon / 60.0) / (remaining + 60.0);
            }
        }
        let (active, _) = self.recent_demand(id, "", now);
        let load = self.inflight(id, "");
        (1.0 + 100.0 * expiry + capacity.ln_1p() / 10.0)
            / (((load + 1) as f64).powi(2) * ((active + 1) as f64).sqrt())
    }

    pub(super) fn recent_demand(&self, id: &str, scope: &str, now: DateTime<Utc>) -> (usize, f64) {
        let mut count = 0;
        let mut rate = 0.0;
        for (key, activity) in &self.state.activity {
            if activity.account_id != id
                || self.state.bindings.get(key).is_none_or(|v| v != id)
                || !applicable(scope, &activity.model)
            {
                continue;
            }
            let mut cadence = activity.cadence_seconds;
            if !cadence.is_finite() || cadence <= 0.0 {
                cadence = 60.0;
            }
            cadence = cadence.clamp(1.0, 1800.0);
            let fresh_for = (3.0 * cadence).clamp(300.0, 1800.0);
            let age = seconds(now - activity.last_request);
            if age < 0.0 || age > fresh_for {
                continue;
            }
            count += 1;
            rate += 1.0 / cadence;
        }
        (count, rate)
    }
    pub(super) fn window_work(&self, id: &str, w: &Window) -> f64 {
        let learning = self
            .state
            .learning
            .get(id)
            .and_then(|m| m.get(&window_key(w)));
        let mut mean = learning.map(|l| l.mean).unwrap_or(0.0);
        if mean <= 0.0 {
            mean = if w.duration > 0 {
                100.0 * NANOS_PER_MINUTE as f64 / w.duration as f64
            } else {
                1.0
            };
        }
        let pending = self
            .completed_for(id, &w.model)
            .saturating_sub(learning.map(|l| l.completed).unwrap_or(0));
        let reserved = (self.inflight(id, &w.model) as f64 + pending as f64) * mean;
        (100.0 - w.used.unwrap_or(0.0) - reserved).max(0.0) / mean
    }
}
