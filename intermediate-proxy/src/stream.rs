use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use proxy_common::cors::cors_headers;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::queue::{Entry, ProxyQueue};
use crate::upstream::Upstream;

/// Status codes that trigger failover to the next proxy.
const RATE_LIMIT_CODES: &[u16] = &[421, 429, 500, 502, 503];
/// Status codes that mean "skip this proxy for this request" without penalty.
const SKIP_CODES: &[u16] = &[403];

/// Response headers we strip when forwarding to the client.
///
/// Keeping the outgoing header block small is important: downstream nginx
/// installations often default to a 4–8 KiB `proxy_buffer_size` and reject
/// the response with `upstream sent too big header` otherwise. We drop:
///  - body/framing headers that we recompute (content-length, transfer-encoding, content-encoding);
///  - CORS — we emit our own, and duplicates blow up the header block;
///  - Cloudflare edge chaff that the client doesn't need;
///  - reporting / timing headers that can carry multi-KiB JSON payloads;
///  - CSP / security hints that are meaningless for a proxied asset.
const SKIP_RESPONSE_HEADERS: &[&str] = &[
    // framing
    "content-encoding",
    "content-length",
    "transfer-encoding",
    // CORS (we set our own)
    "access-control-allow-origin",
    "access-control-allow-methods",
    "access-control-allow-headers",
    "access-control-allow-credentials",
    "access-control-expose-headers",
    "access-control-max-age",
    // Cloudflare edge chaff
    "cf-ray",
    "cf-cache-status",
    "cf-request-id",
    "cf-apo-via",
    "cf-bgj",
    "cf-polished",
    "cf-edge-cache",
    // Reporting / timing (can be large JSON)
    "report-to",
    "reporting-endpoints",
    "nel",
    "expect-ct",
    "server-timing",
    // Protocol upgrade hints (confuse HTTP/1 clients going through nginx)
    "alt-svc",
    "alternate-protocol",
    // Security policies we don't want to propagate through the proxy
    "content-security-policy",
    "content-security-policy-report-only",
    "x-frame-options",
];

/// Buffer size for the body streaming channel.
const STREAM_CHUNK_BUFFER: usize = 64;

/// Forward a request through the proxy queue with failover and mid-stream
/// recovery.
pub async fn forward_with_failover(
    queue: Arc<ProxyQueue>,
    snapshot: Arc<Vec<Arc<Entry>>>,
    method: &Method,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let upstream_timeout = queue.upstream_timeout();
    let mut last_error: Option<String> = None;
    let mut tried: HashSet<String> = HashSet::new();

    for (index, entry) in snapshot.iter().enumerate() {
        tried.insert(entry.url.clone());
        debug!(
            "→ upstream {} [{}] lat={}ms",
            entry.url,
            entry.tier().as_str(),
            entry.avg_latency_ms.load(std::sync::atomic::Ordering::Relaxed)
        );

        let start = Instant::now();
        let result = tokio::time::timeout(
            upstream_timeout,
            send_to_upstream(&entry.upstream, method, headers, body.clone(), None),
        )
        .await;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(Ok(response)) => {
                let status_code = response.status().as_u16();

                if RATE_LIMIT_CODES.contains(&status_code) {
                    debug!(
                        "upstream {} replied {} in {}ms (soft-fail)",
                        entry.url, status_code, elapsed_ms
                    );
                    queue.record_soft_error(
                        entry,
                        elapsed_ms,
                        &format!("status {status_code}"),
                    );
                    last_error = Some(format!("status {status_code}"));
                    continue;
                }

                if SKIP_CODES.contains(&status_code) {
                    debug!(
                        "upstream {} returned skip code {} ({}ms)",
                        entry.url, status_code, elapsed_ms
                    );
                    last_error = Some(format!("skipped {status_code}"));
                    continue;
                }

                debug!(
                    "upstream {} success {} in {}ms",
                    entry.url, status_code, elapsed_ms
                );
                queue.record_success(entry, elapsed_ms);

                let can_recover =
                    *method == Method::GET && can_recover_stream(headers, &response);

                return build_streaming_response(
                    response,
                    queue.clone(),
                    snapshot.clone(),
                    index,
                    method,
                    headers,
                    &body,
                    can_recover,
                    tried,
                )
                .await;
            }
            Ok(Err(e)) => {
                warn!(
                    "upstream {} connection error in {}ms: {}",
                    entry.url, elapsed_ms, e
                );
                queue.record_hard_error(entry, &format!("{e}"));
                last_error = Some(format!("{e}"));
            }
            Err(_) => {
                warn!(
                    "upstream {} timed out after {}ms",
                    entry.url,
                    upstream_timeout.as_millis()
                );
                queue.record_hard_error(
                    entry,
                    &format!("timeout {}ms", upstream_timeout.as_millis()),
                );
                last_error = Some(format!(
                    "timeout after {}ms",
                    upstream_timeout.as_millis()
                ));
            }
        }
    }

    queue.mark_all_failed();
    let msg = last_error.unwrap_or_else(|| "all upstream proxies exhausted".to_string());
    error!("All upstream proxies failed: {}", msg);
    proxy_common::response::text_response(
        StatusCode::BAD_GATEWAY,
        &format!("Proxy Error: {msg}"),
    )
}

/// Check if the stream can be resumed against another proxy on mid-stream
/// failure (needs a cacheable / media GET).
fn can_recover_stream(req_headers: &HeaderMap, response: &reqwest::Response) -> bool {
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

/// Pipe the upstream body to the client, with optional failover recovery on
/// stream error.
#[allow(clippy::too_many_arguments)]
async fn build_streaming_response(
    response: reqwest::Response,
    queue: Arc<ProxyQueue>,
    snapshot: Arc<Vec<Arc<Entry>>>,
    used_index: usize,
    method: &Method,
    original_headers: &HeaderMap,
    original_body: &Bytes,
    can_recover: bool,
    mut tried: HashSet<String>,
) -> Response {
    let status = response.status();
    let resp_headers = response.headers().clone();

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

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(STREAM_CHUNK_BUFFER);

    let method = method.clone();
    let original_headers = original_headers.clone();
    let original_body = original_body.clone();
    let upstream_timeout = queue.upstream_timeout();

    tokio::spawn(async move {
        let mut stream: BoxStream<'static, Result<Bytes, reqwest::Error>> =
            Box::pin(response.bytes_stream());
        let mut total_bytes: u64 = 0;
        let mut current_url = snapshot[used_index].url.clone();

        loop {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    total_bytes += chunk.len() as u64;
                    if tx.send(Ok(chunk)).await.is_err() {
                        // Client disconnected; drop the upstream stream implicitly.
                        return;
                    }
                }
                Some(Err(e)) => {
                    error!(
                        "stream error on {} after {} bytes: {}",
                        current_url, total_bytes, e
                    );

                    if !can_recover {
                        let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
                        return;
                    }

                    queue.record_hard_error_url(
                        &current_url,
                        &format!("stream error: {e}"),
                    );

                    info!(
                        "stream recovery: resuming at byte {} (excluding tried={})",
                        total_bytes,
                        tried.len()
                    );

                    // Use the live (possibly reordered) snapshot for recovery so
                    // we target proxies in up-to-date priority order.
                    let live = queue.snapshot();
                    let mut recovered = false;

                    for next in live.iter() {
                        if !tried.insert(next.url.clone()) {
                            continue;
                        }

                        info!(
                            "recovery: trying {} [{}]",
                            next.url,
                            next.tier().as_str()
                        );

                        let mut extra_headers = HeaderMap::new();
                        if let Ok(range_val) =
                            HeaderValue::from_str(&format!("bytes={total_bytes}-"))
                        {
                            extra_headers.insert("range", range_val);
                        }

                        let mut recovery_headers = original_headers.clone();
                        recovery_headers.remove("if-none-match");
                        recovery_headers.remove("if-modified-since");

                        let start = Instant::now();
                        let result = tokio::time::timeout(
                            upstream_timeout,
                            send_to_upstream(
                                &next.upstream,
                                &method,
                                &recovery_headers,
                                original_body.clone(),
                                Some(&extra_headers),
                            ),
                        )
                        .await;
                        let elapsed_ms = start.elapsed().as_millis() as u64;

                        match result {
                            Ok(Ok(recovery_resp)) => {
                                let recovery_status = recovery_resp.status().as_u16();
                                if recovery_status == 206 || recovery_status == 200 {
                                    info!(
                                        "recovery established via {} ({}) in {}ms",
                                        next.url, recovery_status, elapsed_ms
                                    );
                                    queue.record_success(next, elapsed_ms);

                                    let mut new_stream: BoxStream<
                                        'static,
                                        Result<Bytes, reqwest::Error>,
                                    > = Box::pin(recovery_resp.bytes_stream());

                                    // Server returned 200 OK (ignored Range): discard
                                    // the bytes already sent.
                                    if recovery_status == 200 && total_bytes > 0 {
                                        warn!(
                                            "range ignored by {} — skipping {} bytes",
                                            next.url, total_bytes
                                        );
                                        let mut skipped: u64 = 0;
                                        while skipped < total_bytes {
                                            match new_stream.next().await {
                                                Some(Ok(chunk)) => {
                                                    let remaining = total_bytes - skipped;
                                                    let chunk_len = chunk.len() as u64;
                                                    if chunk_len <= remaining {
                                                        skipped += chunk_len;
                                                    } else {
                                                        let keep =
                                                            &chunk[remaining as usize..];
                                                        total_bytes += keep.len() as u64;
                                                        if tx
                                                            .send(Ok(Bytes::copy_from_slice(
                                                                keep,
                                                            )))
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

                                    stream = new_stream;
                                    current_url = next.url.clone();
                                    recovered = true;
                                    break;
                                } else if RATE_LIMIT_CODES.contains(&recovery_status) {
                                    warn!(
                                        "recovery: {} replied {} in {}ms (soft-fail)",
                                        next.url, recovery_status, elapsed_ms
                                    );
                                    queue.record_soft_error(
                                        next,
                                        elapsed_ms,
                                        &format!("recovery status {recovery_status}"),
                                    );
                                } else {
                                    warn!(
                                        "recovery: {} returned {} in {}ms",
                                        next.url, recovery_status, elapsed_ms
                                    );
                                    queue.record_hard_error(
                                        next,
                                        &format!("recovery status {recovery_status}"),
                                    );
                                }
                            }
                            Ok(Err(re)) => {
                                warn!(
                                    "recovery: {} connection failed in {}ms: {}",
                                    next.url, elapsed_ms, re
                                );
                                queue.record_hard_error(next, &format!("recovery: {re}"));
                            }
                            Err(_) => {
                                warn!(
                                    "recovery: {} timed out after {}ms",
                                    next.url,
                                    upstream_timeout.as_millis()
                                );
                                queue.record_hard_error(next, "recovery timeout");
                            }
                        }
                    }

                    if !recovered {
                        error!("all recovery attempts failed; closing stream");
                        return;
                    }
                }
                None => return,
            }
        }
    });

    let body_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = Body::from_stream(body_stream);

    builder.body(body).unwrap()
}

/// Dispatch a single upstream request based on the upstream kind.
async fn send_to_upstream(
    upstream: &Upstream,
    method: &Method,
    headers: &HeaderMap,
    body: Bytes,
    extra_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, reqwest::Error> {
    use crate::upstream::UpstreamKind;

    match &upstream.kind {
        UpstreamKind::Endpoint { url, client } => {
            crate::upstream::endpoint::forward(
                client,
                url,
                method,
                headers,
                body,
                extra_headers,
            )
            .await
        }
        UpstreamKind::HttpProxy { client } => {
            let target_url = proxy_common::target::decode_target(
                headers
                    .get("x-target")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
            )
            .unwrap_or_default();
            crate::upstream::http_proxy::forward(
                client,
                &target_url,
                method,
                headers,
                body,
                extra_headers,
            )
            .await
        }
        UpstreamKind::Socks5 { client } => {
            let target_url = proxy_common::target::decode_target(
                headers
                    .get("x-target")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
            )
            .unwrap_or_default();
            crate::upstream::socks::forward(
                client,
                &target_url,
                method,
                headers,
                body,
                extra_headers,
            )
            .await
        }
    }
}
