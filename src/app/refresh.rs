use super::*;
use crate::{
    core::Scheduler,
    model::{Account, Snapshot},
    provider::{Manager, UsageRateLimit},
};
use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct RefreshResult {
    pub account: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    deferred: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct RefreshStatus {
    pub attempted_at: DateTime<Utc>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    rate_limited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry_at: Option<DateTime<Utc>>,
}

impl RefreshStatus {
    fn deferral(&self, account: &str, now: DateTime<Utc>) -> Option<RefreshResult> {
        let age = now - self.attempted_at;
        let reason = if self.retry_at.is_some_and(|at| now < at) {
            "rate_limited"
        } else if self.ok && age >= chrono::Duration::zero() && age < chrono::Duration::seconds(30)
        {
            "recent"
        } else {
            return None;
        };
        Some(RefreshResult {
            account: account.into(),
            ok: reason == "recent",
            error: self.error.clone(),
            deferred: reason.into(),
            retry_at: self.retry_at,
        })
    }
}

pub(super) fn read_status(home: &Path) -> BTreeMap<String, RefreshStatus> {
    std::fs::read(home.join("quota-refresh.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn record(home: &Path, result: &RefreshResult) -> Result<()> {
    let _guard = config::lock(home, "quota-refresh")?;
    let mut entries = read_status(home);
    let mut error = String::new();
    for ch in result.error.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if error.len() + ch.len_utf8() > 240 {
            break;
        }
        error.push(ch);
    }
    entries.insert(
        result.account.clone(),
        RefreshStatus {
            attempted_at: Utc::now(),
            ok: result.ok,
            error,
            rate_limited: result.retry_at.is_some(),
            retry_at: result.retry_at,
        },
    );
    config::write(
        home.join("quota-refresh.json"),
        &serde_json::to_vec(&entries)?,
    )
}

pub(crate) struct Refresher<'a> {
    pub home: &'a Path,
    pub scheduler: Option<&'a Scheduler>,
    pub provider: &'a Manager,
}

impl Refresher<'_> {
    pub async fn refresh(&self, id: &str) -> Result<Vec<RefreshResult>> {
        let c = config::load(self.home)?;
        if !id.is_empty() {
            accounts::find(&c, id)?;
        }
        if let Some(s) = self.scheduler {
            s.set_accounts(c.accounts.clone())?;
        }
        let accounts = c.accounts.into_iter().filter(|a| {
            if id.is_empty() {
                a.enabled || a.authenticated
            } else {
                a.id == id
            }
        });
        let mut results = stream::iter(
            accounts
                .enumerate()
                .map(|(i, a)| async move { (i, self.read_account(&a, false).await) }),
        )
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
        results.sort_by_key(|(i, _)| *i);
        if let Some(s) = self.scheduler {
            s.set_accounts(config::load(self.home)?.accounts)?;
        }
        results.into_iter().map(|(_, result)| result).collect()
    }

    pub(crate) async fn read_account(
        &self,
        a: &Account,
        after_rejection: bool,
    ) -> Result<RefreshResult> {
        // Serialize the cooldown check, provider request and diagnostic write for one
        // account across CLI and daemon callers. Other accounts keep making progress.
        config::validate_id(&a.id)?;
        let lock_name = format!("quota-read-{}", a.id);
        let _guard = tokio::time::timeout(
            Duration::from_secs(55),
            config::lock_async(self.home, &lock_name),
        )
        .await
        .context("waiting for account quota refresh")??;
        let _permit = self.provider.refresh_slots.acquire().await?;
        if let Some(result) = read_status(self.home)
            .get(&a.id)
            .and_then(|last| last.deferral(&a.id, Utc::now()))
            && !(after_rejection && result.deferred == "recent")
        {
            return Ok(result);
        }
        let read = tokio::time::timeout(Duration::from_secs(25), async {
            if a.email.is_empty()
                && let Ok(identity) = self.provider.identity(a).await
                && identity.authenticated
                && !identity.email.is_empty()
            {
                let _ = config::mutate(self.home, |c| {
                    if let Some(entry) = c.accounts.iter_mut().find(|v| v.id == a.id) {
                        entry.email = identity.email;
                        entry.authenticated = true;
                    }
                    Ok(())
                });
            }
            self.provider.refresh(a).await
        })
        .await
        .context("refresh timed out")
        .and_then(|result| result)
        .and_then(|snapshot| self.apply(snapshot));
        let mut result = RefreshResult {
            account: a.id.clone(),
            ok: read.is_ok(),
            ..Default::default()
        };
        if let Err(error) = read {
            result.retry_at = error.downcast_ref::<UsageRateLimit>().map(|e| e.until);
            result.error = error.to_string();
        }
        record(self.home, &result)?;
        Ok(result)
    }

    fn apply(&self, snapshot: Snapshot) -> Result<()> {
        if let Some(s) = self.scheduler {
            return s.update(snapshot);
        }
        // The provider read is finished. Lock and reload the latest saved scheduler
        // before merging, so concurrent offline CLI reads cannot overwrite each other.
        let _guard = config::lock(self.home, "daemon")
            .context("live refresh failed and scheduler is locked")?;
        let c = config::load(self.home)?;
        Scheduler::new(self.home.join("scheduler.json"), c.accounts)?.update(snapshot)
    }
}
