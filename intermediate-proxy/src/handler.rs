use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::HeaderValue;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use base64::Engine;
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
    let target = match req
        .headers()
        .get("x-target")
        .and_then(|v| v.to_str().ok())
        .and_then(decode_target)
    {
        Some(t) => t,
        None => return text_response(StatusCode::BAD_REQUEST, "Missing or invalid X-Target"),
    };
    serve(state, target, req, None).await
}

pub async fn x_target_handler(
    State(state): State<Arc<AppState>>,
    Path(encoded): Path<String>,
    req: Request<Body>,
) -> Response {
    let target = match decode_target(encoded.trim_end_matches('/')) {
        Some(t) => t,
        None => {
            return text_response(
                StatusCode::BAD_REQUEST,
                "Invalid /x-target/<base64> path segment",
            )
        }
    };
    let self_host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    serve(state, target, req, self_host).await
}

async fn serve(
    state: Arc<AppState>,
    target: String,
    req: Request<Body>,
    rewrite_to_self: Option<String>,
) -> Response {
    let method = req.method().clone();
    let mut headers = req.headers().clone();

    // Endpoint-upstreams (simple-proxy / workers) read the target from the
    // X-Target header. With the /x-target/<base64> path form the client never
    // sends that header, so re-derive it from the decoded target. Idempotent
    // for the header form (same value re-set).
    let encoded = base64::engine::general_purpose::STANDARD.encode(target.as_bytes());
    if let Ok(v) = HeaderValue::from_str(&encoded) {
        headers.insert("x-target", v);
    }

    let (host, path) = match Url::parse(&target) {
        Ok(u) => (u.host_str().unwrap_or("").to_string(), u.path().to_string()),
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

    debug!("{} {}{} → route '{}'", method, host, path, route.name);

    let target_url = target.clone();
    let mut response = dispatch::run(
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
    .await;

    if let Some(self_host) = rewrite_to_self {
        rewrite_location_to_self(&mut response, &target_url, &self_host);
    }
    response
}

fn rewrite_location_to_self(resp: &mut Response, target_url: &str, self_host: &str) {
    if !resp.status().is_redirection() {
        return;
    }
    let Some(loc) = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
    else {
        return;
    };

    let absolute = if loc.starts_with("http://") || loc.starts_with("https://") {
        loc
    } else {
        let Ok(base) = Url::parse(target_url) else {
            return;
        };
        let Ok(joined) = base.join(&loc) else {
            return;
        };
        joined.to_string()
    };

    if let Ok(u) = Url::parse(&absolute) {
        if u.host_str() == Some(self_host) {
            return;
        }
    }

    let encoded = base64::engine::general_purpose::STANDARD.encode(absolute.as_bytes());
    let new_loc = format!("https://{self_host}/x-target/{encoded}");
    if let Ok(v) = HeaderValue::from_str(&new_loc) {
        resp.headers_mut().insert("location", v);
    }
}