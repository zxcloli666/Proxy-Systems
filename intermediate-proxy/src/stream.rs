use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use proxy_common::cors::cors_headers;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::forward::{describe_reqwest_error, send_to_upstream};
use crate::proxy::ProxyPool;
use crate::route::config::RouteDef;
use crate::route::state::{entry_tier, Tier};
use crate::util::{now_ms, sanitize_url};

const STREAM_CHUNK_BUFFER: usize = 64;

/// Response headers we strip when forwarding to the client. Keeping the
/// outgoing header block small matters: downstream nginx defaults to a 4–8 KiB
/// `proxy_buffer_size` and rejects oversized header blocks.
const SKIP_RESPONSE_HEADERS: &[&str] = &[
    "content-encoding",
    "content-length",
    "transfer-encoding",
    "access-control-allow-origin",
    "access-control-allow-methods",
    "access-control-allow-headers",
    "access-control-allow-credentials",
    "access-control-expose-headers",
    "access-control-max-age",
    "cf-ray",
    "cf-cache-status",
    "cf-request-id",
    "cf-apo-via",
    "cf-bgj",
    "cf-polished",
    "cf-edge-cache",
    "report-to",
    "reporting-endpoints",
    "nel",
    "expect-ct",
    "server-timing",
    "alt-svc",
    "alternate-protocol",
    "content-security-policy",
    "content-security-policy-report-only",
    "x-frame-options",
];

pub fn is_html_response(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            let l = ct.to_ascii_lowercase();
            l.starts_with("text/html") || l.starts_with("application/xhtml")
        })
        .unwrap_or(false)
}

/// Whether a broken stream can be resumed against another proxy (cacheable /
/// media GET). The proxy decides this itself — it isn't Lua-configurable, the
/// streaming layer already knows what's safe to resume with a `Range`.
pub fn can_recover_stream(req_headers: &HeaderMap, response: &reqwest::Response) -> bool {
    let has_cache_headers = req_headers.contains_key("cache-control")
        || req_headers.contains_key("pragma")
        || req_headers.contains_key("if-none-match")
        || req_headers.contains_key("if-modified-since");
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_media = content_type.starts_with("audio/") || content_type.starts_with("video/");
    has_cache_headers || is_media
}

fn copy_headers(status: StatusCode, resp_headers: &reqwest::header::HeaderMap) -> http::response::Builder {
    let mut builder = Response::builder().status(status);
    let hdrs = builder.headers_mut().unwrap();
    for (name, value) in cors_headers() {
        hdrs.insert(name, value);
    }
    for (name, value) in resp_headers.iter() {
        if !SKIP_RESPONSE_HEADERS.contains(&name.as_str()) {
            hdrs.append(name.clone(), value.clone());
        }
    }
    builder
}

/// Build a non-streaming response from a fully buffered body (used when a
/// route enabled `capture_body_bytes`, so we already read the whole body).
pub fn buffered_response(
    status: StatusCode,
    resp_headers: &reqwest::header::HeaderMap,
    body: Bytes,
) -> Response {
    copy_headers(status, resp_headers)
        .body(Body::from(body))
        .unwrap()
}

/// Pipe the upstream body to the client, resuming against another proxy on
/// mid-stream failure when `can_recover`.
#[allow(clippy::too_many_arguments)]
pub async fn build_streaming_response(
    response: reqwest::Response,
    pool: Arc<ProxyPool>,
    route: Arc<RouteDef>,
    used_url: String,
    method: &Method,
    original_headers: &HeaderMap,
    original_body: &Bytes,
    can_recover: bool,
    mut tried: HashSet<String>,
) -> Response {
    let status = response.status();
    let resp_headers = response.headers().clone();
    let builder = copy_headers(status, &resp_headers);

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(STREAM_CHUNK_BUFFER);

    let method = method.clone();
    let original_headers = original_headers.clone();
    let original_body = original_body.clone();
    let timeout = Duration::from_millis(route.timeout_ms);

    tokio::spawn(async move {
        let mut stream: BoxStream<'static, Result<Bytes, reqwest::Error>> =
            Box::pin(response.bytes_stream());
        let mut total_bytes: u64 = 0;
        let mut current_url = used_url;

        loop {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    total_bytes += chunk.len() as u64;
                    if tx.send(Ok(chunk)).await.is_err() {
                        return;
                    }
                }
                Some(Err(e)) => {
                    let reason = describe_reqwest_error(&e);
                    error!(
                        "stream error on {} after {} bytes: {}",
                        sanitize_url(&current_url),
                        total_bytes,
                        reason
                    );
                    if !can_recover {
                        let _ = tx.send(Err(std::io::Error::other(reason))).await;
                        return;
                    }
                    if let Some(e) = pool.find(&current_url) {
                        e.route_state(route.id).observe_failure(
                            None,
                            &format!("stream error: {reason}"),
                            route.slow_ms,
                        );
                    }

                    let live = pool.snapshot();
                    let now = now_ms();
                    let mut recovered = false;

                    for next in live.iter() {
                        let st = next.route_state(route.id);
                        if entry_tier(next, &st, &route, now) >= Tier::Banned {
                            continue;
                        }
                        if !tried.insert(next.url.clone()) {
                            continue;
                        }
                        let mut extra = HeaderMap::new();
                        if let Ok(rv) = HeaderValue::from_str(&format!("bytes={total_bytes}-")) {
                            extra.insert("range", rv);
                        }
                        let mut rh = original_headers.clone();
                        rh.remove("if-none-match");
                        rh.remove("if-modified-since");

                        let start = Instant::now();
                        let result = tokio::time::timeout(
                            timeout,
                            send_to_upstream(
                                &next.upstream,
                                &method,
                                &rh,
                                original_body.clone(),
                                Some(&extra),
                            ),
                        )
                        .await;
                        let elapsed = start.elapsed().as_millis() as u64;

                        match result {
                            Ok(Ok(rr)) => {
                                let rs = rr.status().as_u16();
                                if rs == 206 || rs == 200 {
                                    info!(
                                        "stream recovery via {} ({}) in {}ms",
                                        sanitize_url(&next.url),
                                        rs,
                                        elapsed
                                    );
                                    st.observe_success(elapsed);
                                    next.global.success_count.fetch_add(1, Ordering::Relaxed);
                                    let mut ns: BoxStream<
                                        'static,
                                        Result<Bytes, reqwest::Error>,
                                    > = Box::pin(rr.bytes_stream());
                                    if rs == 200 && total_bytes > 0 {
                                        let mut skipped: u64 = 0;
                                        while skipped < total_bytes {
                                            match ns.next().await {
                                                Some(Ok(c)) => {
                                                    let rem = total_bytes - skipped;
                                                    let cl = c.len() as u64;
                                                    if cl <= rem {
                                                        skipped += cl;
                                                    } else {
                                                        let keep = &c[rem as usize..];
                                                        total_bytes += keep.len() as u64;
                                                        if tx
                                                            .send(Ok(Bytes::copy_from_slice(keep)))
                                                            .await
                                                            .is_err()
                                                        {
                                                            return;
                                                        }
                                                        skipped = total_bytes;
                                                    }
                                                }
                                                Some(Err(_)) | None => break,
                                            }
                                        }
                                    }
                                    stream = ns;
                                    current_url = next.url.clone();
                                    recovered = true;
                                    break;
                                } else {
                                    st.observe_failure(
                                        Some(elapsed),
                                        &format!("recovery status {rs}"),
                                        route.slow_ms,
                                    );
                                }
                            }
                            Ok(Err(re)) => {
                                let r = describe_reqwest_error(&re);
                                warn!(
                                    "recovery {} failed in {}ms: {}",
                                    sanitize_url(&next.url),
                                    elapsed,
                                    r
                                );
                                st.observe_failure(
                                    None,
                                    &format!("recovery: {r}"),
                                    route.slow_ms,
                                );
                            }
                            Err(_) => {
                                st.observe_failure(None, "recovery timeout", route.slow_ms);
                            }
                        }
                    }

                    if !recovered {
                        error!("all stream recovery attempts failed; closing");
                        return;
                    }
                }
                None => return,
            }
        }
    });

    let body_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    builder.body(Body::from_stream(body_stream)).unwrap()
}
