//! Native HTTP, SSE and WebSocket forwarding with acceptance-bound affinity.
mod classification;
mod headers;
mod http;
#[cfg(test)]
mod tests;
mod websocket;

use crate::{
    core::{Lease, Scheduler, UnavailableError},
    model::{Account, Credential, Rejection, Snapshot},
    provider::Manager,
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{FromRequestParts, State, WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::future::BoxFuture;
use headers::{header, local_host, valid_origin};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub const MAX_BODY: usize = 256 << 20;
const MAX_PRELUDE: usize = 256 << 10;
const MAX_ATTEMPTS: usize = 16;
const CALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub trait Backend: Send + Sync + 'static {
    fn credentials<'a>(&'a self, account: &'a Account)
    -> BoxFuture<'a, anyhow::Result<Credential>>;
    fn refresh<'a>(&'a self, account: &'a Account) -> BoxFuture<'a, anyhow::Result<Snapshot>>;
}
impl Backend for Manager {
    fn credentials<'a>(
        &'a self,
        account: &'a Account,
    ) -> BoxFuture<'a, anyhow::Result<Credential>> {
        Box::pin(Manager::credentials(self, account))
    }
    fn refresh<'a>(&'a self, account: &'a Account) -> BoxFuture<'a, anyhow::Result<Snapshot>> {
        Box::pin(Manager::refresh(self, account))
    }
}
#[derive(Default)]
struct Lifecycle {
    closed: bool,
    drain_deadline: Option<Instant>,
    active_calls: usize,
    refreshing: HashSet<String>,
}
pub struct Server {
    pub scheduler: Arc<Scheduler>,
    backend: Arc<dyn Backend>,
    secret: String,
    client: reqwest::Client,
    slots: Arc<Semaphore>,
    lifecycle: Mutex<Lifecycle>,
    cancel: CancellationToken,
    workers: TaskTracker,
}
impl Server {
    pub fn new(scheduler: Arc<Scheduler>, backend: Arc<dyn Backend>, secret: String) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(32)
            .connect_timeout(Duration::from_secs(60))
            .no_gzip()
            .build()
            .expect("static HTTP client configuration");
        Self {
            scheduler,
            backend,
            secret,
            client,
            slots: Arc::new(Semaphore::new(128)),
            lifecycle: Mutex::default(),
            cancel: CancellationToken::new(),
            workers: TaskTracker::new(),
        }
    }
    /// The fallback leaves explicitly registered daemon control routes mergeable.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .fallback(Self::handle)
            .with_state(self.clone())
    }
    /// Identical gate for the gateway and daemon control endpoints.
    // Failures are already complete HTTP responses; boxing would add an extra
    // allocation to every rejected local request solely to shrink this enum.
    #[allow(clippy::result_large_err)]
    pub fn authorize(&self, headers: &HeaderMap) -> Result<(), Response> {
        if !local_host(header(headers, "host")) || !valid_origin(headers) {
            return Err(fail(
                StatusCode::FORBIDDEN,
                "only local clients are allowed",
            ));
        }
        let authorization = header(headers, "authorization");
        let mut key = authorization
            .strip_prefix("Bearer ")
            .unwrap_or(authorization);
        if key.is_empty() {
            key = header(headers, "x-api-key");
        }
        // Hash both candidates to fixed-length values before constant-time comparison.
        let supplied = Sha256::digest(key.as_bytes());
        let expected = Sha256::digest(self.secret.as_bytes());
        let mismatch = supplied
            .iter()
            .zip(expected)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b));
        if self.secret.is_empty() || mismatch != 0 {
            return Err(fail(
                StatusCode::UNAUTHORIZED,
                "local authentication required",
            ));
        }
        if self.lifecycle.lock().unwrap().closed {
            return Err(fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "proxy is shutting down",
            ));
        }
        Ok(())
    }
    /// Returns busy without changing admission when work is still active.
    pub fn drain(&self) -> bool {
        let mut state = self.lifecycle.lock().unwrap();
        if state.active_calls > 0 {
            return true;
        }
        state.drain_deadline = Some(Instant::now() + Duration::from_secs(30));
        false
    }
    pub fn resume(&self) {
        self.lifecycle.lock().unwrap().drain_deadline = None;
    }
    pub fn active_calls(&self) -> usize {
        self.lifecycle.lock().unwrap().active_calls
    }
    pub async fn shutdown(&self) {
        {
            let mut state = self.lifecycle.lock().unwrap();
            state.closed = true;
            self.cancel.cancel();
            self.workers.close();
        }
        self.workers.wait().await;
    }
    fn admission_open(state: &mut Lifecycle) -> bool {
        if state
            .drain_deadline
            .is_some_and(|deadline| Instant::now() > deadline)
        {
            state.drain_deadline = None;
        }
        !state.closed && state.drain_deadline.is_none()
    }
    fn begin_call(self: &Arc<Self>) -> Option<CallGuard> {
        let mut state = self.lifecycle.lock().unwrap();
        if !Self::admission_open(&mut state) {
            return None;
        }
        state.active_calls += 1;
        Some(CallGuard(self.clone()))
    }
    fn reject(self: &Arc<Self>, lease: &Lease, rejection: Rejection) -> anyhow::Result<()> {
        if let Err(error) = self.scheduler.reject(lease, rejection) {
            let _ = self.scheduler.complete(lease);
            return Err(error);
        }
        self.refresh_quota(lease.account.clone());
        Ok(())
    }
    fn refresh_quota(self: &Arc<Self>, account: Account) {
        let mut state = self.lifecycle.lock().unwrap();
        if state.closed || state.refreshing.contains(&account.id) || state.refreshing.len() >= 4 {
            return;
        }
        state.refreshing.insert(account.id.clone());
        let server = self.clone();
        self.workers.spawn(async move {
            tokio::select! {
                _ = server.cancel.cancelled() => {},
                result = tokio::time::timeout(Duration::from_secs(25),server.backend.refresh(&account)) => {
                    if let Ok(Ok(snapshot)) = result { let _ = server.scheduler.update(snapshot); }
                }
            }
            server.lifecycle.lock().unwrap().refreshing.remove(&account.id);
        });
    }
    async fn handle(State(server): State<Arc<Self>>, request: Request<Body>) -> Response {
        if let Err(response) = server.authorize(request.headers()) {
            return response;
        }
        let path = request.uri().path();
        if request.method() == Method::GET {
            match path {
                "/_noria/status" | "/status" => {
                    return Json(server.scheduler.status()).into_response();
                }
                "/_noria/health" => {
                    return Json(json!({"ok":true,"version":env!("CARGO_PKG_VERSION")}))
                        .into_response();
                }
                _ => {}
            }
        }
        if request.method() == Method::POST
            && matches!(path, "/_noria/drain" | "/_noria/undrain" | "/_noria/resume")
        {
            let busy = if path == "/_noria/drain" {
                server.drain()
            } else {
                server.resume();
                false
            };
            return Json(json!({"busy":busy})).into_response();
        }
        let Some((provider, path)) = headers::route(path) else {
            return fail(StatusCode::NOT_FOUND, "unknown native API route");
        };
        let permit = {
            let mut state = server.lifecycle.lock().unwrap();
            if !Self::admission_open(&mut state) {
                return updating();
            }
            match server.slots.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => return fail(StatusCode::SERVICE_UNAVAILABLE, "connection limit reached"),
            }
        };
        let (mut parts, body) = request.into_parts();
        let upgrade = header(&parts.headers, "upgrade").eq_ignore_ascii_case("websocket")
            && header(&parts.headers, "connection")
                .split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("upgrade"));
        if upgrade {
            if provider != "codex" || path != "/responses" {
                return fail(
                    StatusCode::BAD_REQUEST,
                    "WebSocket requires the Codex responses route",
                );
            }
            let headers = parts.headers.clone();
            let query = parts.uri.query().unwrap_or_default().to_owned();
            return match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
                Ok(upgrade) => {
                    let token = server.workers.token();
                    upgrade
                        .read_buffer_size(8192)
                        .write_buffer_size(8192)
                        .max_message_size(MAX_BODY)
                        .max_frame_size(MAX_BODY)
                        .on_upgrade(move |socket| async move {
                            let _worker = token;
                            let _permit = permit;
                            server.serve_websocket(socket, headers, query).await;
                        })
                        .into_response()
                }
                Err(error) => error.into_response(),
            };
        }
        let Some(call) = server.begin_call() else {
            return updating();
        };
        if parts.method != Method::POST {
            let mut response = fail(StatusCode::METHOD_NOT_ALLOWED, "POST required");
            response
                .headers_mut()
                .insert("allow", HeaderValue::from_static("POST"));
            return response;
        }
        if header(&parts.headers, "content-length")
            .parse::<u64>()
            .is_ok_and(|size| size > MAX_BODY as u64)
        {
            return fail(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds 256 MiB",
            );
        }
        let deadline = tokio::time::Instant::now() + CALL_TIMEOUT;
        let bytes = tokio::select! {
            _ = server.cancel.cancelled() => return fail(StatusCode::SERVICE_UNAVAILABLE,"proxy is shutting down"),
            read = tokio::time::timeout_at(deadline, to_bytes(body,MAX_BODY)) => match read {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(error)) => {
                    let limit = std::error::Error::source(&error).is_some_and(|e| e.to_string().contains("length limit exceeded"));
                    return if limit { fail(StatusCode::PAYLOAD_TOO_LARGE,"request body exceeds 256 MiB") } else { fail(StatusCode::BAD_REQUEST,"cannot read request") };
                }
                Err(_) => return fail(StatusCode::REQUEST_TIMEOUT,"cannot read request"),
            }
        };
        let object = match parse_request(&bytes) {
            Ok(object) => object,
            Err(_) => return fail(StatusCode::BAD_REQUEST, "JSON object required"),
        };
        let session = headers::session_key(&parts.headers, &object);
        let model = headers::raw_string(object.model);
        server
            .serve_native(
                provider,
                path,
                parts.uri.query().unwrap_or_default(),
                &parts.headers,
                bytes,
                session,
                model,
                CallResources {
                    _call: call,
                    _permit: permit,
                },
                deadline,
            )
            .await
    }
}
struct CallGuard(Arc<Server>);
impl Drop for CallGuard {
    fn drop(&mut self) {
        self.0.lifecycle.lock().unwrap().active_calls -= 1;
    }
}
struct CallResources {
    _call: CallGuard,
    _permit: OwnedSemaphorePermit,
}
struct LeaseGuard {
    scheduler: Arc<Scheduler>,
    lease: Lease,
}
impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let _ = self.scheduler.complete(&self.lease);
    }
}
#[derive(Default, Deserialize)]
struct RequestMeta<'a> {
    #[serde(borrow)]
    model: Option<&'a RawValue>,
    #[serde(borrow)]
    session_id: Option<&'a RawValue>,
    #[serde(borrow)]
    prompt_cache_key: Option<&'a RawValue>,
    #[serde(borrow)]
    metadata: Option<&'a RawValue>,
    #[serde(borrow, rename = "type")]
    kind: Option<&'a RawValue>,
    #[serde(borrow)]
    generate: Option<&'a RawValue>,
}
fn parse_request(bytes: &[u8]) -> anyhow::Result<RequestMeta<'_>> {
    if bytes.iter().find(|v| !v.is_ascii_whitespace()) != Some(&b'{') {
        anyhow::bail!("JSON object required");
    }
    Ok(serde_json::from_slice(bytes)?)
}
fn fail(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({"error":{"type":"noria_error","message":message}})),
    )
        .into_response()
}
fn updating() -> Response {
    let mut response = fail(
        StatusCode::SERVICE_UNAVAILABLE,
        "router is updating; retry shortly",
    );
    response
        .headers_mut()
        .insert("retry-after", HeaderValue::from_static("2"));
    response
}
fn unavailable_details(error: &anyhow::Error) -> (Value, Option<chrono::DateTime<chrono::Utc>>) {
    let mut details = json!({"type":"noria_error","message":"no usable account; inspect noria status or refresh quota"});
    if let Some(unavailable) = error.downcast_ref::<UnavailableError>() {
        details["message"] = json!(unavailable.to_string());
        if let Some(retry) = unavailable.retry_at {
            details["retry_at"] = json!(retry.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        }
        return (details, unavailable.retry_at);
    }
    (details, None)
}
fn fail_unavailable(error: &anyhow::Error) -> Response {
    let (details, retry) = unavailable_details(error);
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error":details})),
    )
        .into_response();
    if let Some(retry) = retry
        && let Ok(value) = HeaderValue::from_str(&httpdate::fmt_http_date(retry.into()))
    {
        response.headers_mut().insert("retry-after", value);
    }
    response
}
