use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use proxy_common::response::text_response;
use proxy_common::target::decode_target;
use std::sync::Arc;
use tracing::info;

use crate::queue::ProxyQueue;
use crate::stream::forward_with_failover;

pub async fn proxy_handler(State(queue): State<Arc<ProxyQueue>>, req: Request<Body>) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();

    // Validate X-Target header
    let target_header = headers.get("x-target").and_then(|v| v.to_str().ok());
    let decoded = target_header.and_then(decode_target);

    if decoded.is_none() {
        return text_response(StatusCode::BAD_REQUEST, "Missing headers");
    }

    info!(
        "Streaming request: {} to {}",
        method,
        decoded.as_deref().unwrap_or("?")
    );

    // Collect body
    let body = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Failed to read request body"),
    };

    // Get queue snapshot
    let snapshot = queue.snapshot().await;

    info!(
        "Request proxy snapshot: {} proxies (version {})",
        snapshot.proxies.len(),
        snapshot.version
    );

    forward_with_failover(
        &queue,
        &snapshot.proxies,
        snapshot.version,
        &method,
        &headers,
        body,
    )
    .await
}
