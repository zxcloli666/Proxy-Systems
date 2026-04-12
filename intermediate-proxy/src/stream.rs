use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use proxy_common::cors::cors_headers;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::queue::ProxyQueue;
use crate::upstream::Upstream;

/// Status codes that trigger failover to the next proxy.
const RATE_LIMIT_CODES: &[u16] = &[429, 500, 503];

/// Response headers to skip when forwarding.
const SKIP_RESPONSE_HEADERS: &[&str] = &["content-encoding", "content-length", "transfer-encoding"];

/// Forward a request through the proxy queue with failover and stream recovery.
pub async fn forward_with_failover(
    queue: &Arc<ProxyQueue>,
    snapshot_proxies: &[Upstream],
    snapshot_version: u64,
    method: &Method,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let mut last_error: Option<String> = None;

    for (i, upstream) in snapshot_proxies.iter().enumerate() {
        info!("Trying proxy: {}", upstream.url);

        let result = send_to_upstream(upstream, method, headers, body.clone(), None).await;

        match result {
            Ok(response) => {
                let status_code = response.status().as_u16();

                if RATE_LIMIT_CODES.contains(&status_code) {
                    warn!("Rate limited ({}) on {}", status_code, upstream.url);
                    queue.mark_error(&upstream.url).await;
                    queue.move_to_end(&upstream.url, snapshot_version).await;
                    last_error = Some(format!("Rate limited: {status_code}"));
                    continue;
                }

                info!("Success via {}", upstream.url);
                queue.mark_success(&upstream.url).await;

                // Determine if stream recovery is possible
                let can_recover = *method == Method::GET && can_recover_stream(headers, &response);

                return build_streaming_response(
                    response,
                    queue,
                    snapshot_proxies,
                    snapshot_version,
                    i,
                    method,
                    headers,
                    &body,
                    can_recover,
                )
                .await;
            }
            Err(e) => {
                warn!("Error with {}: {}", upstream.url, e);
                queue.mark_error(&upstream.url).await;
                queue.move_to_end(&upstream.url, snapshot_version).await;
                last_error = Some(e.to_string());
            }
        }
    }

    queue.mark_all_failed().await;
    let msg = last_error.unwrap_or_else(|| "All proxy servers failed".to_string());
    error!("{}", msg);
    proxy_common::response::text_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        &format!("Proxy Error: {msg}"),
    )
}

/// Check if stream recovery is possible based on request/response headers.
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

/// Build a streaming response with optional failover recovery.
#[allow(clippy::too_many_arguments)]
async fn build_streaming_response(
    response: reqwest::Response,
    queue: &Arc<ProxyQueue>,
    snapshot_proxies: &[Upstream],
    snapshot_version: u64,
    used_index: usize,
    method: &Method,
    original_headers: &HeaderMap,
    original_body: &Bytes,
    can_recover: bool,
) -> Response {
    let status = response.status();
    let resp_headers = response.headers().clone();

    // Build response headers
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

    // Stream body through mpsc channel for failover support
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);

    let queue = Arc::clone(queue);
    let snapshot_proxies = snapshot_proxies.to_vec();
    let method = method.clone();
    let original_headers = original_headers.clone();
    let original_body = original_body.clone();

    tokio::spawn(async move {
        let mut stream: BoxStream<'static, Result<Bytes, reqwest::Error>> =
            Box::pin(response.bytes_stream());
        let mut total_bytes: u64 = 0;
        let mut current_index = used_index;
        let mut current_url = snapshot_proxies[used_index].url.clone();

        loop {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    total_bytes += chunk.len() as u64;
                    if tx.send(Ok(chunk)).await.is_err() {
                        break; // Client disconnected
                    }
                }
                Some(Err(e)) => {
                    error!(
                        "Stream error on {} after {} bytes: {}",
                        current_url, total_bytes, e
                    );

                    if !can_recover {
                        let _ = tx
                            .send(Err(std::io::Error::other(e.to_string())))
                            .await;
                        break;
                    }

                    info!(
                        "Attempting stream recovery (GET + Cache). Resume at byte {}",
                        total_bytes
                    );

                    queue.mark_error(&current_url).await;
                    queue.move_to_end(&current_url, snapshot_version).await;

                    let mut recovered = false;

                    for (j, next) in snapshot_proxies.iter().enumerate().skip(current_index + 1) {
                        info!("Recovery: Trying next proxy {}", next.url);

                        let mut extra_headers = HeaderMap::new();
                        if let Ok(range_val) =
                            HeaderValue::from_str(&format!("bytes={total_bytes}-"))
                        {
                            extra_headers.insert("range", range_val);
                        }

                        // Remove conditional headers for recovery
                        let mut recovery_headers = original_headers.clone();
                        recovery_headers.remove("if-none-match");
                        recovery_headers.remove("if-modified-since");

                        let result = send_to_upstream(
                            next,
                            &method,
                            &recovery_headers,
                            original_body.clone(),
                            Some(&extra_headers),
                        )
                        .await;

                        match result {
                            Ok(recovery_resp) => {
                                let recovery_status = recovery_resp.status().as_u16();
                                if recovery_status == 206 || recovery_status == 200 {
                                    info!(
                                        "Recovery connection established with {} ({})",
                                        next.url, recovery_status
                                    );

                                    let mut new_stream: BoxStream<
                                        'static,
                                        Result<Bytes, reqwest::Error>,
                                    > = Box::pin(recovery_resp.bytes_stream());

                                    // If server returned 200 (ignored Range), skip already-sent bytes
                                    if recovery_status == 200 && total_bytes > 0 {
                                        warn!(
                                            "Server returned 200 OK (Range ignored). Skipping {} bytes",
                                            total_bytes
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

                                    stream = new_stream;
                                    current_index = j;
                                    current_url = next.url.clone();
                                    queue.mark_success(&next.url).await;
                                    recovered = true;
                                    break;
                                } else {
                                    warn!(
                                        "Recovery failed: {} returned status {}",
                                        next.url, recovery_status
                                    );
                                    queue.mark_error(&next.url).await;
                                }
                            }
                            Err(e) => {
                                warn!("Recovery connection failed to {}: {}", next.url, e);
                                queue.mark_error(&next.url).await;
                            }
                        }
                    }

                    if !recovered {
                        error!("All recovery attempts failed. Aborting stream.");
                        break;
                    }
                }
                None => break, // Stream ended normally
            }
        }
    });

    let body_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = Body::from_stream(body_stream);

    builder.body(body).unwrap()
}

/// Send a request to an upstream proxy.
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
            crate::upstream::endpoint::forward(client, url, method, headers, body, extra_headers)
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
