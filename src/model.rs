//! Shared account and quota vocabulary. JSON remains compatible with Go Noria.
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

pub fn zero_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(1, 1, 1, 0, 0, 0).unwrap()
}

pub(crate) fn is_false(v: &bool) -> bool {
    !*v
}
pub(crate) fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// Go encoded zero time values even on fields marked omitempty. Read those
/// values as None while preserving real timezone-aware timestamps.
pub(crate) mod optional_time {
    use super::*;
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Option<DateTime<Utc>>, D::Error> {
        Ok(Option::<DateTime<Utc>>::deserialize(d)?.filter(|t| *t != zero_time()))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Account {
    pub id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub email: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub alias: String,
    #[serde(skip_serializing_if = "is_false")]
    pub authenticated: bool,
    #[serde(
        deserialize_with = "optional_time::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_login: Option<DateTime<Utc>>,
    pub provider: String,
    pub profile: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
}

impl Account {
    pub fn display_name(&self) -> &str {
        if !self.alias.is_empty() {
            &self.alias
        } else if !self.email.is_empty() {
            &self.email
        } else {
            &self.id
        }
    }
    pub fn label(&self) -> &str {
        if !self.alias.is_empty() {
            &self.alias
        } else if !self.email.is_empty() {
            &self.email
        } else {
            "Sign-in required"
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Window {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub limit_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(rename = "used_percent", skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(
        deserialize_with = "optional_time::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub resets_at: Option<DateTime<Utc>>,
    /// Go time.Duration's wire unit is nanoseconds.
    #[serde(skip_serializing_if = "is_zero")]
    pub duration: i64,
    #[serde(skip_serializing_if = "is_false")]
    pub exhausted: bool,
    #[serde(
        rename = "ordinary_usage_allowed",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed: Option<bool>,
    #[serde(rename = "awaiting_first_use", skip_serializing_if = "is_false")]
    pub dormant: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Snapshot {
    pub account_id: String,
    pub observed_at: DateTime<Utc>,
    pub available: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub auth_invalid: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<Window>,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            account_id: String::new(),
            observed_at: zero_time(),
            available: false,
            auth_invalid: false,
            windows: vec![],
        }
    }
}

#[derive(Clone, Default)]
pub struct Credential {
    pub token: String,
    pub account_id: String,
}
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential").finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Rejection {
    pub kind: String,
    pub model: String,
    pub window: String,
    pub until: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_zero_times_and_presentation_identity() {
        let a: Account = serde_json::from_str(
            r#"{"id":"immutable","email":"u@example.com","last_login":"0001-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(a.last_login, None);
        assert_eq!(a.display_name(), "u@example.com");
        let mut renamed = a.clone();
        renamed.alias = "Work".into();
        assert_eq!(renamed.display_name(), "Work");
        assert_eq!(renamed.id, a.id);
    }
}
