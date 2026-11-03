//! Atomic quota-aware scheduling. Persistent state contains metadata only.
mod quota;
mod scheduler;
#[cfg(test)]
mod tests;

use crate::{
    config,
    model::{self, Account, Snapshot},
};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

const TELEMETRY_AGE: Duration = Duration::minutes(2);
const UNKNOWN_CONCURRENCY: usize = 2;
pub const NANOS_PER_MINUTE: i64 = 60_000_000_000;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Blocker {
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub window: String,
    pub created_at: DateTime<Utc>,
    #[serde(
        deserialize_with = "model::optional_time::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub until: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct Learning {
    mean: f64,
    samples: u64,
    used: f64,
    completed: u64,
    observed_at: DateTime<Utc>,
    #[serde(
        deserialize_with = "model::optional_time::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    reset_at: Option<DateTime<Utc>>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct SessionActivity {
    account_id: String,
    model: String,
    last_request: DateTime<Utc>,
    cadence_seconds: f64,
}

fn null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
struct DiskState {
    version: u32,
    #[serde(deserialize_with = "null_default")]
    bindings: HashMap<String, String>,
    #[serde(deserialize_with = "null_default")]
    activity: HashMap<String, SessionActivity>,
    #[serde(deserialize_with = "null_default")]
    blockers: HashMap<String, Vec<Blocker>>,
    #[serde(deserialize_with = "null_default")]
    snapshots: HashMap<String, Snapshot>,
    #[serde(deserialize_with = "null_default")]
    learning: HashMap<String, HashMap<String, Learning>>,
    #[serde(deserialize_with = "null_default")]
    completed: HashMap<String, u64>,
    #[serde(deserialize_with = "null_default")]
    completed_models: HashMap<String, HashMap<String, u64>>,
    #[serde(rename = "predicted_resets", deserialize_with = "null_default")]
    clocks: HashMap<String, HashMap<String, DateTime<Utc>>>,
    #[serde(deserialize_with = "null_default")]
    window_observed: HashMap<String, HashMap<String, DateTime<Utc>>>,
}
impl Default for DiskState {
    fn default() -> Self {
        Self {
            version: 1,
            bindings: HashMap::new(),
            activity: HashMap::new(),
            blockers: HashMap::new(),
            snapshots: HashMap::new(),
            learning: HashMap::new(),
            completed: HashMap::new(),
            completed_models: HashMap::new(),
            clocks: HashMap::new(),
            window_observed: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Lease {
    pub id: String,
    pub account: Account,
}
#[derive(Clone)]
struct Reservation {
    lease: Lease,
    binding: String,
    model: String,
    accepted: bool,
    previous_binding: Option<String>,
    rebind: bool,
}

pub struct Scheduler {
    inner: Mutex<Inner>,
    now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}
struct Inner {
    path: PathBuf,
    accounts: Vec<Account>,
    state: DiskState,
    leases: HashMap<String, Reservation>,
}

#[derive(Debug)]
pub struct UnavailableError {
    pub provider: String,
    pub model: String,
    pub retry_at: Option<DateTime<Utc>>,
}
impl std::fmt::Display for UnavailableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.retry_at {
            Some(at) => write!(
                f,
                "no {} account available; retry after {}",
                self.provider,
                at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ),
            None => write!(
                f,
                "no {} account available; refresh quota or check account authentication",
                self.provider
            ),
        }
    }
}
impl std::error::Error for UnavailableError {}

impl Scheduler {
    pub fn read_status(path: impl AsRef<Path>, accounts: Vec<Account>) -> Result<Status> {
        Ok(Self::new(path, accounts)?.status())
    }
    pub fn new(path: impl AsRef<Path>, accounts: Vec<Account>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let state = if path.as_os_str().is_empty() {
            DiskState::default()
        } else {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    serde_json::from_slice::<DiskState>(&bytes).context("decode scheduler state")?
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => DiskState::default(),
                Err(e) => return Err(e).context("read scheduler state"),
            }
        };
        if state.version != 1 {
            bail!("unsupported scheduler state version: {}", state.version);
        }
        let s = Self {
            inner: Mutex::new(Inner {
                path,
                accounts: vec![],
                state,
                leases: HashMap::new(),
            }),
            now: Arc::new(Utc::now),
        };
        s.set_accounts(accounts)?;
        Ok(s)
    }
    fn guard(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}
impl Inner {
    fn save(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        config::write_with(&self.path, |file| {
            let mut writer = std::io::BufWriter::with_capacity(32 << 10, file);
            serde_json::to_writer(&mut writer, &self.state)?;
            std::io::Write::flush(&mut writer)?;
            Ok(())
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountStatus {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub email: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub alias: String,
    pub display_name: String,
    pub id: String,
    pub label: String,
    pub provider: String,
    pub enabled: bool,
    pub inflight: usize,
    pub recent_sessions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<Blocker>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub predicted_resets: HashMap<String, DateTime<Utc>>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub window_observed: HashMap<String, DateTime<Utc>>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub bindings: usize,
    pub accounts: Vec<AccountStatus>,
}
