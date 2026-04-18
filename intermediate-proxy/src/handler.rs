use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use proxy_common::response::text_response;
use proxy_common::target::decode_target;
use std::sync::Arc;
use tracing::debug;

use crate::queue::ProxyQueue;
use crate::stream::forward_with_failover;

pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

pub async fn proxy_handler(
    State(queue): State<Arc<ProxyQueue>>,
    req: Request<Body>,
) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();

    let target_header = headers.get("x-target").and_then(|v| v.to_str().ok());
    let decoded = target_header.and_then(decode_target);

    let Some(target) = decoded else {
        return text_response(StatusCode::BAD_REQUEST, "Missing headers");
    };

    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Failed to read request body"),
    };

    let snapshot = queue.snapshot();

    debug!(
        "{} {} ({} bytes) → {} upstream(s)",
        method,
        target,
        body.len(),
        snapshot.len()
    );

    forward_with_failover(Arc::clone(&queue), snapshot, &method, &headers, body).await
}
