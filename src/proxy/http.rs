use super::{
    classification::{classify, meaningful_event, sse_data},
    headers::{endpoint, response_headers, upstream_headers},
    *,
};
use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt};
use std::{io, pin::Pin};
use tokio::time::{Instant as TokioInstant, timeout_at};

impl Server {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn serve_native(
        self: &Arc<Self>,
        provider: &str,
        path: &str,
        query: &str,
        headers: &HeaderMap,
        body: Bytes,
        session: String,
        requested_model: String,
        resources: CallResources,
        deadline: TokioInstant,
    ) -> Response {
        let mut excluded = HashSet::new();
        let mut needs_claude_login = false;
        for attempt in 0..MAX_ATTEMPTS {
            let lease = match self.scheduler.acquire(
                provider,
                &session,
                &requested_model,
                &excluded,
            ) {
                Ok(lease) => LeaseGuard {
                    scheduler: self.scheduler.clone(),
                    lease,
                },
                Err(error) => {
                    if provider == "claude" && needs_claude_login {
                        return fail(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "Claude account authentication expired; sign in again using Noria account relogin",
                        );
                    }
                    return fail_unavailable(&error);
                }
            };
            let credential = tokio::select! {
                _ = self.cancel.cancelled() => return fail(StatusCode::SERVICE_UNAVAILABLE,"proxy is shutting down"),
                result = timeout_at(deadline,self.backend.credentials(&lease.lease.account)) => match result {
                    Ok(Ok(credential)) if !credential.token.is_empty() => credential,
                    result => {
                        if let Ok(Err(error)) = result { needs_claude_login |= error.to_string().contains("sign in again"); }
                        excluded.insert(lease.lease.account.id.clone()); continue;
                    }
                }
            };
            let Ok(address) = endpoint(&lease.lease.account, path, query) else {
                return fail(StatusCode::BAD_GATEWAY, "invalid upstream endpoint");
            };
            let Ok(upstream_headers) = upstream_headers(headers, provider, &credential) else {
                return fail(StatusCode::BAD_GATEWAY, "invalid upstream credentials");
            };
            let request = self
                .client
                .post(address)
                .headers(upstream_headers)
                .body(body.clone());
            let response = tokio::select! {
                _ = self.cancel.cancelled() => return fail(StatusCode::SERVICE_UNAVAILABLE,"proxy is shutting down"),
                result = timeout_at(deadline.min(TokioInstant::now()+Duration::from_secs(60)),request.send()) => match result {
                    Ok(Ok(response)) => response,
                    _ => return fail(StatusCode::BAD_GATEWAY,"upstream connection failed"),
                }
            };
            let status = response.status();
            let response_header = response.headers().clone();
            let mut upstream = Upstream {
                stream: Box::pin(response.bytes_stream()),
                pending: Bytes::new(),
                ended: false,
                cancel: self.cancel.clone(),
                deadline,
            };
            if status.as_u16() >= 400 {
                let prefix = upstream.prefix(MAX_PRELUDE).await;
                let (rejection, retry) =
                    classify(status.as_u16(), &response_header, &prefix, &requested_model);
                if retry && attempt + 1 < MAX_ATTEMPTS {
                    if self.reject(&lease.lease, rejection).is_err() {
                        return fail(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "cannot persist account rejection",
                        );
                    }
                    excluded.insert(lease.lease.account.id.clone());
                    continue;
                }
                if !rejection.kind.is_empty() {
                    let _ = self.scheduler.reject(&lease.lease, rejection);
                }
                return stream_response(
                    status,
                    response_header,
                    prefix,
                    upstream,
                    resources,
                    lease,
                );
            }
            let mut prefix = Bytes::new();
            if header(&response_header, "content-type")
                .to_ascii_lowercase()
                .contains("text/event-stream")
            {
                let inspection = upstream.inspect_sse(&requested_model).await;
                prefix = inspection.bytes;
                if inspection.retry && attempt + 1 < MAX_ATTEMPTS {
                    if self.reject(&lease.lease, inspection.rejection).is_err() {
                        return fail(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "cannot persist account rejection",
                        );
                    }
                    excluded.insert(lease.lease.account.id.clone());
                    continue;
                }
                if inspection.retry {
                    let _ = self.scheduler.reject(&lease.lease, inspection.rejection);
                } else if inspection.closed && prefix.is_empty() {
                    return fail(
                        StatusCode::BAD_GATEWAY,
                        "upstream stream closed before response",
                    );
                } else if self.scheduler.accept(&lease.lease).is_err() {
                    return fail(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "cannot persist session affinity",
                    );
                }
            } else if self.scheduler.accept(&lease.lease).is_err() {
                return fail(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "cannot persist session affinity",
                );
            }
            return stream_response(status, response_header, prefix, upstream, resources, lease);
        }
        fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "account retry limit reached",
        )
    }
}

struct Upstream {
    stream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    pending: Bytes,
    ended: bool,
    cancel: CancellationToken,
    deadline: TokioInstant,
}
impl Upstream {
    async fn next(&mut self) -> io::Result<Option<Bytes>> {
        if !self.pending.is_empty() {
            return Ok(Some(std::mem::take(&mut self.pending)));
        }
        if self.ended {
            return Ok(None);
        }
        let next = tokio::select! {
            _ = self.cancel.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted,"proxy shutdown")),
            result = timeout_at(self.deadline,self.stream.next()) => match result {
                Ok(Some(Ok(bytes))) => Ok(Some(bytes)),
                Ok(None) => Ok(None),
                Ok(Some(Err(_))) => Err(io::Error::other("upstream response interrupted")),
                Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut,"upstream response timeout")),
            }
        };
        if !matches!(next, Ok(Some(_))) {
            self.ended = true;
        }
        next
    }
    async fn prefix(&mut self, limit: usize) -> Bytes {
        let mut held = BytesMut::new();
        while held.len() < limit {
            match self.next().await {
                Ok(Some(mut chunk)) => {
                    let count = chunk.len().min(limit - held.len());
                    held.extend_from_slice(&chunk.split_to(count));
                    self.pending = chunk;
                }
                _ => break,
            }
        }
        held.freeze()
    }
    // Buffer only lifecycle events and bounded bytes. Remaining chunk data stays
    // zero-copy and is polled only when the downstream is ready to receive it.
    async fn inspect_sse(&mut self, model: &str) -> Inspection {
        let mut held = BytesMut::new();
        let mut event_start = 0;
        let mut line_start = 0;
        let empty_headers = HeaderMap::new();
        loop {
            match self.next().await {
                Ok(Some(mut chunk)) => {
                    while !chunk.is_empty() {
                        let count = chunk
                            .iter()
                            .position(|v| *v == b'\n')
                            .map_or(chunk.len(), |index| index + 1)
                            .min(MAX_PRELUDE - held.len());
                        held.extend_from_slice(&chunk.split_to(count));
                        if held.len() >= MAX_PRELUDE {
                            self.pending = chunk;
                            return Inspection::plain(held.freeze(), false);
                        }
                        if held.last() == Some(&b'\n') {
                            let line = &held[line_start..];
                            if line.iter().all(u8::is_ascii_whitespace) {
                                let data = sse_data(&held[event_start..]);
                                let (rejection, retry) = classify(0, &empty_headers, &data, model);
                                if retry || meaningful_event(&data) {
                                    self.pending = chunk;
                                    return Inspection {
                                        bytes: held.freeze(),
                                        rejection,
                                        retry,
                                        closed: false,
                                    };
                                }
                                event_start = held.len();
                            }
                            line_start = held.len();
                        }
                    }
                }
                _ => {
                    let data = sse_data(&held[event_start..]);
                    let (rejection, retry) = classify(0, &empty_headers, &data, model);
                    return Inspection {
                        bytes: held.freeze(),
                        rejection,
                        retry,
                        closed: true,
                    };
                }
            }
        }
    }
}
struct Inspection {
    bytes: Bytes,
    rejection: Rejection,
    retry: bool,
    closed: bool,
}
impl Inspection {
    fn plain(bytes: Bytes, closed: bool) -> Self {
        Self {
            bytes,
            rejection: Rejection::default(),
            retry: false,
            closed,
        }
    }
}
fn stream_response(
    status: StatusCode,
    headers: HeaderMap,
    prefix: Bytes,
    mut upstream: Upstream,
    resources: CallResources,
    lease: LeaseGuard,
) -> Response {
    let stream = async_stream::stream! {
        let _resources = resources;
        let _lease = lease;
        if !prefix.is_empty() { yield Ok::<_,io::Error>(prefix); }
        loop {
            match upstream.next().await {
                Ok(Some(chunk)) => { if !chunk.is_empty() { yield Ok(chunk); } },
                Ok(None) => break,
                Err(error) => { yield Err(error); break; },
            }
        }
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers(&headers);
    response
}
