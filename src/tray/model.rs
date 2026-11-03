use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct Dashboard {
    pub schema_version: u32,
    pub version: String,
    pub running: bool,
    pub accounts: Vec<Account>,
    pub client_roots: Vec<ClientRoot>,
    pub integrations: Vec<Integration>,
    pub preferences: Preferences,
}

impl Dashboard {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let value: Self = serde_json::from_slice(bytes)?;
        ensure!(
            value.schema_version == 1,
            "unsupported dashboard schema {}",
            value.schema_version
        );
        Ok(value)
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub start_at_login: bool,
    pub tray_at_login: bool,
    pub auto_update: bool,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct Account {
    pub id: String,
    pub email: String,
    pub alias: String,
    pub provider: String,
    pub enabled: bool,
    pub snapshot: Option<Snapshot>,
}

impl Account {
    pub fn title(&self) -> &str {
        if !self.alias.is_empty() {
            &self.alias
        } else if !self.email.is_empty() {
            &self.email
        } else {
            "Sign-in needed"
        }
    }

    // Keep the same two unscoped windows as Swift and the original Go tray.
    // Missing, invalid, and model-specific quota values never become invented usage.
    pub fn quota_rows(&self, now: DateTime<Utc>) -> Vec<String> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        [
            ("5h", 18_000_000_000_000_i64),
            ("Week", 604_800_000_000_000),
        ]
        .into_iter()
        .filter_map(|(label, duration)| {
            snapshot
                .windows
                .iter()
                .find(|window| {
                    window.duration == duration
                        && window.model.is_empty()
                        && window.used_percent.is_some_and(|value| {
                            value.is_finite() && (0.0..=100.0).contains(&value)
                        })
                })
                .map(|window| window.text(label, now))
        })
        .collect()
    }

    pub fn stale(&self, now: DateTime<Utc>) -> bool {
        self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .observed_at
                .is_none_or(|observed| (now - observed).num_seconds() > 150)
        })
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub observed_at: Option<DateTime<Utc>>,
    pub windows: Vec<QuotaWindow>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct QuotaWindow {
    pub model: String,
    pub used_percent: Option<f64>,
    pub duration: i64,
    pub resets_at: Option<DateTime<Utc>>,
}

impl QuotaWindow {
    fn text(&self, label: &str, now: DateTime<Utc>) -> String {
        let left = format!("{:.0}% left", 100.0 - self.used_percent.unwrap_or_default());
        let suffix = match self.resets_at {
            None => "reset unknown".to_owned(),
            Some(reset) if reset <= now => "refreshing reset".to_owned(),
            Some(reset) => {
                let minutes = ((reset - now).num_milliseconds() as f64 / 60_000.0).ceil() as i64;
                let countdown = if minutes >= 1440 {
                    format!("{}d {}h", minutes / 1440, minutes % 1440 / 60)
                } else if minutes >= 60 {
                    format!("{}h {}m", minutes / 60, minutes % 60)
                } else {
                    format!("{minutes}m")
                };
                format!("resets in {countdown}")
            }
        };
        format!("{label}  {left} · {suffix}")
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct ClientRoot {
    pub provider: String,
    pub root: String,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct Integration {
    pub provider: String,
    pub path: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct UpdateResult {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub updated: bool,
    pub message: String,
}

#[derive(Deserialize)]
pub struct HealthReport {
    pub checks: Vec<HealthCheck>,
}

#[derive(Deserialize)]
pub struct HealthCheck {
    pub name: String,
    pub ok: bool,
    #[serde(default)]
    pub detail: String,
}

pub fn provider_name(provider: &str) -> &str {
    match provider {
        "codex" => "Codex",
        "claude" => "Claude Code",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn swift_fixture_and_schema_contract() {
        let dashboard =
            Dashboard::parse(include_bytes!("../../apps/menubar/Tests/status.json")).unwrap();
        assert_eq!(dashboard.accounts[0].title(), "alex@example.com");
        assert_eq!(dashboard.accounts[0].quota_rows(Utc::now()).len(), 2);
        assert!(Dashboard::parse(br#"{"schema_version":2}"#).is_err());
    }

    #[test]
    fn quota_rejects_missing_model_scoped_and_invalid_values() {
        let now = Utc::now();
        let window = QuotaWindow {
            duration: 18_000_000_000_000,
            used_percent: Some(25.0),
            resets_at: Some(now + Duration::seconds(61)),
            ..Default::default()
        };
        let mut account = Account {
            snapshot: Some(Snapshot {
                observed_at: Some(now),
                windows: vec![window],
            }),
            ..Default::default()
        };
        assert_eq!(account.quota_rows(now), ["5h  75% left · resets in 2m"]);
        for value in [
            None,
            Some(-1.0),
            Some(101.0),
            Some(f64::NAN),
            Some(f64::INFINITY),
        ] {
            account.snapshot.as_mut().unwrap().windows[0].used_percent = value;
            assert!(account.quota_rows(now).is_empty());
        }
        let window = &mut account.snapshot.as_mut().unwrap().windows[0];
        window.used_percent = Some(25.0);
        window.model = "model-scoped".to_owned();
        assert!(account.quota_rows(now).is_empty());
    }

    #[test]
    fn aliases_never_change_routing_ids() {
        let account = Account {
            id: "stable-id".into(),
            email: "mail@example.com".into(),
            alias: "Personal".into(),
            ..Default::default()
        };
        assert_eq!(account.title(), "Personal");
        assert_eq!(account.id, "stable-id");
    }
}
