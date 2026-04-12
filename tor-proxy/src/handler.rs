use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use proxy_common::cors::cors_headers;
use proxy_common::headers::filter_request_headers;
use proxy_common::response::text_response;
use proxy_common::target::decode_target;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};
use url::Url;

use crate::pool::NodePool;
use crate::tunnel::http_over_socks5;

/// Response headers to skip.
const SKIP_RESPONSE_HEADERS: &[&str] = &[
    "content-security-policy",
    "x-frame-options",
    "transfer-encoding",
];

/// Rate limit status codes that trigger failover.
const RATE_LIMIT_CODES: &[u16] = &[429, 500, 503];

#[derive(Clone)]
pub struct HandlerState {
    pub pool: Arc<NodePool>,
    pub socks_timeout: Duration,
    pub request_timeout: Duration,
}

pub async fn proxy_handler(State(state): State<HandlerState>, req: Request<Body>) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();

    let target_header = match headers.get("x-target").and_then(|v| v.to_str().ok()) {
        Some(v) => v.to_string(),
        None => {
            return text_response(
                StatusCode::BAD_REQUEST,
                "Missing or invalid X-Target header",
            )
        }
    };

    let target_url = match decode_target(&target_header) {
        Some(url) => url,
        None => {
            return text_response(
                StatusCode::BAD_REQUEST,
                "Missing or invalid X-Target header",
            )
        }
    };

    info!("{} {}", method, target_url);

    // Collect body
    let body_bytes = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Failed to read request body"),
    };

    let target_parsed = match Url::parse(&target_url) {
        Ok(u) => u,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Invalid target URL"),
    };

    // Prepare headers for the target
    let filtered = filter_request_headers(&headers, target_parsed.host_str().unwrap_or(""));
    let header_pairs: Vec<(String, String)> = filtered
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    let body_opt = if method != "GET" && method != "HEAD" && !body_bytes.is_empty() {
        Some(body_bytes)
    } else {
        None
    };

    let ordered = state.pool.get_ordered_nodes().await;
    let mut last_error: Option<String> = None;

    for (node_idx, node_config) in &ordered {
        info!("  -> {}", node_config.host);

        match http_over_socks5(
            &node_config.host,
            node_config.socks_port,
            &target_url,
            method.as_str(),
            header_pairs.clone(),
            body_opt.clone(),
            state.socks_timeout,
            state.request_timeout,
        )
        .await
        {
            Ok((status_code, resp_headers, resp_body)) => {
                if RATE_LIMIT_CODES.contains(&status_code) {
                    info!("  <- {} {} (retry next)", status_code, node_config.host);
                    state.pool.on_error(*node_idx).await;
                    last_error = Some(format!("{status_code} from {}", node_config.host));
                    continue;
                }

                state.pool.on_success(*node_idx).await;
                info!("  <- {} via {}", status_code, node_config.host);

                return build_response(status_code, &resp_headers, resp_body, &target_url);
            }
            Err(e) => {
                warn!("  x {}: {}", node_config.host, e);
                state.pool.on_error(*node_idx).await;
                last_error = Some(e.to_string());
            }
        }
    }

    error!("All {} tor nodes failed", ordered.len());
    let msg = last_error.unwrap_or_else(|| "all nodes failed".to_string());
    text_response(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!("Tor proxy error: {msg}"),
    )
}

fn build_response(
    status_code: u16,
    resp_headers: &[(String, String)],
    body: Bytes,
    target_url: &str,
) -> Response {
    let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = Response::builder().status(status);

    let hdrs = builder.headers_mut().unwrap();
    for (name, value) in cors_headers() {
        hdrs.insert(name, value);
    }

    for (name, value) in resp_headers {
        let name_lower = name.to_lowercase();
        if SKIP_RESPONSE_HEADERS.contains(&name_lower.as_str()) {
            continue;
        }

        // Handle redirects
        if name_lower == "location" && status.is_redirection() {
            let resolved = resolve_location(value, target_url);
            if let Ok(loc) = HeaderValue::from_str(&resolved) {
                hdrs.insert("location", loc);
            }
            continue;
        }

        if let (Ok(n), Ok(v)) = (
            axum::http::header::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            hdrs.append(n, v);
        }
    }

    builder.body(Body::from(body)).unwrap()
}

fn resolve_location(location: &str, base_url: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    if location.starts_with("//") {
        if let Ok(base) = Url::parse(base_url) {
            return format!("{}:{}", base.scheme(), location);
        }
    }
    if let Ok(base) = Url::parse(base_url) {
        if let Ok(resolved) = base.join(location) {
            return resolved.to_string();
        }
    }
    location.to_string()
}
