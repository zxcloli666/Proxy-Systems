use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use proxy_common::response::text_response;
use proxy_common::target::decode_target;
use std::sync::Arc;
use tracing::debug;
use url::Url;

use crate::dispatch;
use crate::route::select_route;
use crate::AppState;

pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

pub async fn proxy_handler(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();

    let target = match headers
        .get("x-target")
        .and_then(|v| v.to_str().ok())
        .and_then(decode_target)
    {
        Some(t) => t,
        None => return text_response(StatusCode::BAD_REQUEST, "Missing or invalid X-Target"),
    };

    let (host, path) = match Url::parse(&target) {
        Ok(u) => (
            u.host_str().unwrap_or("").to_string(),
            u.path().to_string(),
        ),
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Invalid target URL"),
    };

    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Failed to read request body"),
    };

    let cfg = state.lua.config();
    let pairs: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_ascii_lowercase(), s.to_string()))
        })
        .collect();
    let probe_ctx = crate::lua::HookCtx {
        route_name: String::new(),
        target_url: target.clone(),
        host: host.clone(),
        path: path.clone(),
        method: method.to_string(),
        headers: pairs,
        attempt: 1,
        proxy_url: None,
        proxy_tags: Vec::new(),
        elapsed_ms: 0,
    };

    let Some(route) = select_route(&state.lua, &cfg, &host, &path, method.as_str(), &probe_ctx)
    else {
        return text_response(
            StatusCode::BAD_GATEWAY,
            &format!("Proxy Error: no route matched {host}{path}"),
        );
    };

    debug!(
        "{} {}{} → route '{}'",
        method, host, path, route.name
    );

    dispatch::run(
        Arc::clone(&state.pool),
        Arc::clone(&state.lua),
        route,
        dispatch::Request {
            method,
            headers,
            body,
            target_url: target,
            host,
            path,
        },
        state.max_hard_attempts,
    )
    .await
}
