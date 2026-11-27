use super::*;
use crate::model::{Rejection, Window};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

impl Scheduler {
    /// Reauthentication clears only older authentication failures, preserving quota and affinity.
    pub fn set_accounts(&self, accounts: Vec<Account>) -> Result<()> {
        let mut seen = HashSet::new();
        for account in &accounts {
            if account.id.is_empty() || account.provider.is_empty() {
                bail!("account ID and provider are required");
            }
            if !seen.insert(&account.id) {
                bail!("duplicate account ID {:?}", account.id);
            }
        }
        let mut s = self.guard();
        for account in &accounts {
            if let Some(at) = account.last_login {
                if let Some(blocks) = s.state.blockers.get_mut(&account.id) {
                    blocks.retain(|b| b.kind != "auth" || b.created_at > at);
                }
                if let Some(snapshot) = s.state.snapshots.get_mut(&account.id)
                    && snapshot.auth_invalid
                    && snapshot.observed_at <= at
                {
                    snapshot.auth_invalid = false;
                }
            }
        }
        s.accounts = accounts;
        Ok(())
    }

    /// Selection and reservation share one lock, including provisional affinity.
    pub fn acquire(
        &self,
        provider: &str,
        session: &str,
        model: &str,
        exclude: &HashSet<String>,
    ) -> Result<Lease> {
        let mut s = self.guard();
        let now = (self.now)();
        let key = binding_key(provider, session);
        let previous_binding = s.state.bindings.get(&key).cloned();
        let bound = previous_binding.clone().or_else(|| {
            if key.is_empty() {
                None
            } else {
                s.leases
                    .values()
                    .find(|r| r.binding == key)
                    .map(|r| r.lease.account.id.clone())
            }
        });
        let mut chosen = None;
        let mut fallback = None;
        let mut best = f64::NEG_INFINITY;
        let mut fallback_best = f64::NEG_INFINITY;
        let mut earliest = None;
        for (index, a) in s.accounts.iter().enumerate() {
            if !a.enabled || a.provider != provider || exclude.contains(&a.id) {
                continue;
            }
            let (mut usable, known, mut retry) = s.eligibility(&a.id, model, now, false);
            if usable
                && !known
                && bound.as_deref() != Some(&a.id)
                && s.inflight(&a.id, "") >= UNKNOWN_CONCURRENCY
            {
                usable = false;
                retry = Some(now + Duration::seconds(1));
            }
            if !usable {
                if let Some(at) = retry
                    && earliest.is_none_or(|old| at < old)
                {
                    earliest = Some(at);
                }
                continue;
            }
            if bound.as_deref() == Some(&a.id) {
                chosen = Some(index);
                break;
            }
            let score = s.score(&a.id, model, now);
            if known && score > best {
                chosen = Some(index);
                best = score;
            }
            if !known && score > fallback_best {
                fallback = Some(index);
                fallback_best = score;
            }
        }
        let index = chosen.or(fallback).ok_or_else(|| UnavailableError {
            provider: provider.into(),
            model: model.into(),
            retry_at: earliest,
        })?;
        let account = s.accounts[index].clone();
        let rebind = bound.as_deref().is_none_or(|bound| {
            bound == account.id
                || !s.accounts.iter().any(|a| {
                    a.id == bound
                        && a.enabled
                        && a.provider == provider
                        && s.eligibility(bound, model, now, true).0
                })
        });
        let lease = Lease {
            id: uuid::Uuid::new_v4().simple().to_string(),
            account,
        };
        s.leases.insert(
            lease.id.clone(),
            Reservation {
                lease: lease.clone(),
                binding: key,
                model: model.into(),
                accepted: false,
                previous_binding,
                rebind,
            },
        );
        Ok(lease)
    }

    pub fn accept(&self, lease: &Lease) -> Result<()> {
        let mut s = self.guard();
        let r = s
            .leases
            .get(&lease.id)
            .context("unknown or completed lease")?
            .clone();
        if r.accepted {
            return Ok(());
        }
        let now = (self.now)();
        let id = &r.lease.account.id;
        let old_id = s.state.bindings.get(&r.binding).cloned();
        let change_binding = !r.binding.is_empty() && r.rebind && old_id == r.previous_binding;
        if change_binding {
            s.state.bindings.insert(r.binding.clone(), id.clone());
        }
        let old_clocks = s.state.clocks.get(id).cloned();
        s.start_clocks(id, &r.model, now);
        let old_activity = s.state.activity.get(&r.binding).cloned();
        if !r.binding.is_empty() && s.state.bindings.get(&r.binding) == Some(id) {
            let mut activity = SessionActivity {
                account_id: id.clone(),
                model: r.model.clone(),
                last_request: now,
                cadence_seconds: 60.0,
            };
            if let Some(old) = &old_activity {
                let elapsed = seconds(now - old.last_request);
                if elapsed > 0.0 && elapsed < 1800.0 {
                    let prior = if old.cadence_seconds > 0.0 {
                        old.cadence_seconds
                    } else {
                        60.0
                    };
                    activity.cadence_seconds = 0.7 * prior + 0.3 * elapsed.max(1.0);
                }
            }
            s.state.activity.insert(r.binding.clone(), activity);
        }
        if let Err(err) = s.save() {
            restore(&mut s.state.activity, &r.binding, old_activity);
            if change_binding {
                restore(&mut s.state.bindings, &r.binding, old_id);
            }
            restore(&mut s.state.clocks, id, old_clocks);
            return Err(err);
        }
        s.leases.get_mut(&lease.id).unwrap().accepted = true;
        Ok(())
    }

    pub fn reject(&self, lease: &Lease, rejection: Rejection) -> Result<()> {
        let mut s = self.guard();
        let r = s
            .leases
            .get(&lease.id)
            .context("unknown or completed lease")?
            .clone();
        if r.accepted {
            bail!("cannot retry an already accepted lease");
        }
        let now = (self.now)();
        let kind = rejection.kind;
        if !["quota", "auth", "throughput", "transient"].contains(&kind.as_str()) {
            bail!("unknown rejection kind {:?}", kind);
        }
        let mut until = rejection.until;
        if until.is_none() {
            until = match kind.as_str() {
                "transient" => Some(now + Duration::seconds(1)),
                "throughput" => Some(now + Duration::seconds(2)),
                "quota" => Some(
                    s.quota_reset(
                        &r.lease.account.id,
                        &rejection.model,
                        &rejection.window,
                        now,
                    )
                    .unwrap_or(now + Duration::minutes(1)),
                ),
                _ => None,
            };
        }
        if until.is_some_and(|at| at <= now) {
            until = Some(now + Duration::seconds(1));
        }
        let mut b = Blocker {
            kind,
            source: "backend".into(),
            model: rejection.model,
            window: rejection.window,
            created_at: now,
            until,
        };
        let id = &r.lease.account.id;
        let previous = s.state.blockers.get(id).cloned();
        let mut blocks = previous.clone().unwrap_or_default();
        if let Some(old) = blocks.iter_mut().find(|o| {
            o.kind == b.kind && o.source == b.source && o.model == b.model && o.window == b.window
        }) {
            if old.until.is_none() || (b.until.is_some() && old.until > b.until) {
                b.until = old.until;
            }
            *old = b;
        } else {
            blocks.push(b);
        }
        s.state.blockers.insert(id.clone(), blocks);
        if let Err(err) = s.save() {
            restore(&mut s.state.blockers, id, previous);
            return Err(err);
        }
        s.leases.remove(&lease.id);
        Ok(())
    }

    /// Release once even when persistence fails; retain real completed work in memory.
    pub fn complete(&self, lease: &Lease) -> Result<()> {
        let mut s = self.guard();
        let Some(r) = s.leases.remove(&lease.id) else {
            return Ok(());
        };
        if r.accepted {
            *s.state
                .completed
                .entry(r.lease.account.id.clone())
                .or_default() += 1;
            *s.state
                .completed_models
                .entry(r.lease.account.id)
                .or_default()
                .entry(r.model)
                .or_default() += 1;
            s.save()?;
        }
        Ok(())
    }
}

pub(super) fn restore<T>(map: &mut HashMap<String, T>, key: &str, value: Option<T>) {
    match value {
        Some(v) => {
            map.insert(key.into(), v);
        }
        None => {
            map.remove(key);
        }
    }
}
pub(super) fn seconds(duration: Duration) -> f64 {
    duration.num_milliseconds() as f64 / 1000.0
}
pub(super) fn binding_key(provider: &str, session: &str) -> String {
    if session.is_empty() {
        return String::new();
    }
    let mut h = Sha256::new();
    h.update(provider.as_bytes());
    h.update([0]);
    h.update(session.as_bytes());
    format!("{:x}", h.finalize())
}
pub fn applicable(scope: &str, name: &str) -> bool {
    if scope.is_empty() {
        return true;
    }
    let normalize = |v: &str| {
        v.to_lowercase()
            .replace(['_', ' '], "-")
            .trim_matches('-')
            .to_owned()
    };
    let scope = normalize(scope);
    let name = normalize(name);
    scope == name
        || (["sonnet", "opus", "haiku", "fable"].contains(&scope.as_str())
            && format!("-{name}-").contains(&format!("-{scope}-")))
}
pub(super) fn window_key(w: &Window) -> String {
    format!(
        "{}\0{}\0{}\0{}",
        w.limit_id,
        w.model,
        w.name,
        go_duration(w.duration)
    )
}
fn go_duration(nanos: i64) -> String {
    if nanos == 0 {
        return "0s".into();
    }
    let n = nanos.unsigned_abs();
    let sign = if nanos < 0 { "-" } else { "" };
    if n < 1_000_000_000 {
        let (div, unit) = if n < 1_000 {
            (1, "ns")
        } else if n < 1_000_000 {
            (1_000, "µs")
        } else {
            (1_000_000, "ms")
        };
        let digits = if div == 1 {
            n.to_string()
        } else {
            decimal(n / div, n % div, if div == 1_000 { 3 } else { 6 })
        };
        return format!("{sign}{digits}{unit}");
    }
    let sec = n / 1_000_000_000;
    let frac = n % 1_000_000_000;
    let mut out = String::from(sign);
    if sec >= 3600 {
        out.push_str(&format!("{}h", sec / 3600));
    }
    if sec >= 60 {
        out.push_str(&format!("{}m", (sec / 60) % 60));
    }
    out.push_str(&decimal(sec % 60, frac, 9));
    out.push('s');
    out
}
fn decimal(integer: u64, remainder: u64, width: usize) -> String {
    if remainder == 0 {
        integer.to_string()
    } else {
        format!(
            "{integer}.{}",
            format!("{remainder:0width$}").trim_end_matches('0')
        )
    }
}
pub(super) fn same_window(name: &str, w: &Window) -> bool {
    name.is_empty() || name.eq_ignore_ascii_case(&w.name)
}

impl Inner {
    pub(super) fn inflight(&self, id: &str, scope: &str) -> usize {
        self.leases
            .values()
            .filter(|l| l.lease.account.id == id && applicable(scope, &l.model))
            .count()
    }
    pub(super) fn completed_for(&self, id: &str, scope: &str) -> u64 {
        if scope.is_empty() {
            return self.state.completed.get(id).copied().unwrap_or(0);
        }
        self.state
            .completed_models
            .get(id)
            .map(|m| {
                m.iter()
                    .filter(|(name, _)| applicable(scope, name))
                    .map(|(_, n)| n)
                    .sum()
            })
            .unwrap_or(0)
    }
    pub(super) fn start_clocks(&mut self, id: &str, model: &str, now: DateTime<Utc>) {
        if let Some(snapshot) = self.state.snapshots.get(id) {
            for w in &snapshot.windows {
                if !applicable(&w.model, model)
                    || !w.dormant
                    || w.resets_at.is_some()
                    || w.duration <= 0
                {
                    continue;
                }
                let clocks = self.state.clocks.entry(id.into()).or_default();
                let key = window_key(w);
                if clocks.get(&key).is_none_or(|at| *at <= now) {
                    clocks.insert(key, now + Duration::nanoseconds(w.duration));
                }
            }
        }
    }
    pub(super) fn quota_reset(
        &self,
        id: &str,
        model: &str,
        name: &str,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        self.state
            .snapshots
            .get(id)?
            .windows
            .iter()
            .filter(|w| {
                applicable(&w.model, model)
                    && same_window(name, w)
                    && (!name.is_empty() || super::quota::window_exhausted(w))
            })
            .filter_map(|w| w.resets_at)
            .filter(|at| *at > now)
            .max()
    }
}
