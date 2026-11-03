use super::headers::header;
use crate::model::Rejection;
use axum::http::HeaderMap;
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

pub(super) fn classify(
    mut status: u16,
    headers: &HeaderMap,
    body: &[u8],
    requested_model: &str,
) -> (Rejection, bool) {
    let event = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    if status == 0 {
        status = event
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or_default();
    }
    let mut error = event
        .pointer("/response/error")
        .filter(|v| v.is_object())
        .unwrap_or(&event);
    for key in ["body", "error"] {
        if let Some(inner) = error.get(key).filter(|v| v.is_object()) {
            error = inner;
        }
    }
    let string = |key| error.get(key).and_then(Value::as_str).unwrap_or("");
    let marker = format!("{} {}", string("code"), string("type")).to_lowercase();
    let message = string("message").to_lowercase();
    let contains = |values: &[&str]| values.iter().any(|v| message.contains(v));
    let short = contains(&["5-hour", "five-hour", "5 hour", "five hour", "five_hour"]);
    let weekly = contains(&[
        "7-day",
        "seven-day",
        "7 day",
        "seven day",
        "weekly",
        "seven_day",
    ]);
    let subscription = short
        || weekly
        || contains(&[
            "usage limit",
            "usage quota",
            "hit your limit",
            "hit your usage limit",
            "reached your limit",
            "rate limit resets",
            "extra usage",
            "plan limit",
        ])
        || (contains(&[
            "limit has been reached",
            "your limit was reached",
            "reached your",
        ]) && message.contains("reset"));
    let mut rejection = Rejection::default();
    if short && !weekly {
        rejection.window = "five_hour".into();
    } else if weekly && !short {
        rejection.window = "seven_day".into();
    }
    if marker.contains("model") {
        rejection.model = requested_model.into();
    }
    rejection.kind = if status == 401
        || ["authentication_error", "invalid_api_key", "token_expired"]
            .iter()
            .any(|v| marker.contains(v))
    {
        rejection.model.clear();
        "auth"
    } else if [
        "usage_limit_reached",
        "usage_limit_exceeded",
        "insufficient_quota",
        "quota_exceeded",
        "model_limit_reached",
    ]
    .iter()
    .any(|v| marker.contains(v))
    {
        "quota"
    } else if status == 429 || marker.contains("rate_limit") {
        if subscription { "quota" } else { "throughput" }
    } else if status >= 500 {
        "transient"
    } else {
        return (rejection, false);
    }
    .into();
    if status == 429 {
        let shared = ["5h", "7d"].iter().any(|window| {
            header(
                headers,
                &format!("anthropic-ratelimit-unified-{window}-status"),
            )
            .eq_ignore_ascii_case("rejected")
        });
        let mut rejected = 0;
        for window in ["5h", "7d", "7d_oi"] {
            if window == "7d_oi" && shared {
                continue;
            }
            let prefix = format!("anthropic-ratelimit-unified-{window}");
            if !header(headers, &format!("{prefix}-status")).eq_ignore_ascii_case("rejected") {
                continue;
            }
            rejection.kind = "quota".into();
            rejected += 1;
            rejection.window = if window == "5h" {
                "five_hour"
            } else {
                "seven_day"
            }
            .into();
            if window == "7d_oi" {
                rejection.model = requested_model.into();
            }
            if let Some(reset) = header(headers, &format!("{prefix}-reset"))
                .parse::<i64>()
                .ok()
                .and_then(|v| DateTime::from_timestamp(v, 0))
            {
                rejection.until = Some(
                    rejection
                        .until
                        .map_or(reset, |previous| previous.max(reset)),
                );
            }
        }
        if rejected > 1 {
            rejection.window.clear();
        }
    }
    if let Some(seconds) = error
        .get("resets_at")
        .and_then(Value::as_f64)
        .filter(|v| *v > 0.0)
    {
        rejection.until = DateTime::from_timestamp(seconds as i64, 0);
    }
    if rejection.until.is_none()
        && let Some(seconds) = error
            .get("resets_in_seconds")
            .and_then(Value::as_f64)
            .filter(|v| *v > 0.0)
    {
        rejection.until =
            Utc::now().checked_add_signed(Duration::seconds((seconds as i64).min(i64::MAX / 1000)));
    }
    if rejection.until.is_none() {
        rejection.until = retry_after(header(headers, "retry-after"));
    }
    if rejection.kind == "throughput" && rejection.until.is_none() {
        rejection.until = Some(Utc::now() + Duration::seconds(2));
    }
    let retry = matches!(rejection.kind.as_str(), "quota" | "auth");
    (rejection, retry)
}
fn retry_after(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(seconds) = value.parse::<f64>()
        && seconds.is_finite()
        && seconds > 0.0
    {
        return Utc::now().checked_add_signed(Duration::milliseconds(
            (seconds * 1000.0).min(i64::MAX as f64) as i64,
        ));
    }
    httpdate::parse_http_date(value).ok().map(DateTime::from)
}
pub(super) fn meaningful_event(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    // Only a valid native lifecycle schema is harmless. Malformed content must
    // commit immediately, just as Go's typed decoder did, to forbid replay of
    // output we cannot prove is an empty lifecycle notification. IgnoredAny
    // validates array elements without allocating generated content strings.
    #[derive(serde::Deserialize)]
    struct LifecycleMessage {
        content: Option<Vec<serde::de::IgnoredAny>>,
    }
    #[derive(serde::Deserialize)]
    struct LifecycleEvent {
        #[serde(rename = "type")]
        kind: Option<String>,
        message: Option<LifecycleMessage>,
    }
    let Ok(event) = serde_json::from_slice::<LifecycleEvent>(data) else {
        return true;
    };
    let kind = event.kind.as_deref().unwrap_or_default();
    if kind == "message_start"
        && event
            .message
            .as_ref()
            .and_then(|message| message.content.as_ref())
            .is_none_or(Vec::is_empty)
    {
        return false;
    }
    !matches!(kind, "response.created" | "response.in_progress" | "ping")
}
pub(super) fn sse_data(event: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    for line in event.split(|v| *v == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(value) = line.strip_prefix(b"data:") {
            if !data.is_empty() {
                data.push(b'\n');
            }
            data.extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
        }
    }
    data
}
