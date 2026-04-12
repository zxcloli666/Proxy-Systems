use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::response::Response;
use proxy_common::cors::cors_headers;
use proxy_common::headers::{filter_request_headers, filter_response_headers};
use proxy_common::response::text_response;
use proxy_common::target::decode_target;
use reqwest::Client;
use tracing::info;
use url::Url;

use crate::redirect::resolve_redirect;

pub async fn proxy_handler(
    State(client): State<Client>,
    req: Request<Body>,
) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();

    let target_header = match headers.get("x-target").and_then(|v| v.to_str().ok()) {
        Some(v) => v.to_string(),
        None => return text_response(StatusCode::BAD_REQUEST, "Missing X-Target header"),
    };

    let target_url = match decode_target(&target_header) {
        Some(url) => url,
        None => return text_response(StatusCode::BAD_REQUEST, "Invalid base64 encoded URL"),
    };

    info!("Proxying {} to {}", method, target_url);

    let target_parsed = match Url::parse(&target_url) {
        Ok(u) => u,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Invalid target URL"),
    };

    let filtered_headers = filter_request_headers(&headers, target_parsed.host_str().unwrap_or(""));

    // Collect request body
    let body_bytes = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Failed to read request body"),
    };

    // Build reqwest request
    let mut req_builder = client
        .request(method.clone(), &target_url)
        .headers(reqwest_headers(&filtered_headers));

    if method != Method::GET && method != Method::HEAD && !body_bytes.is_empty() {
        req_builder = req_builder.body(body_bytes);
    }

    let response = match req_builder.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Proxy error: {}", e);
            return text_response(StatusCode::SERVICE_UNAVAILABLE, &format!("Proxy Error: {e}"));
        }
    };

    let status = response.status();
    let resp_headers = response.headers().clone();

    // Handle redirects
    if status.is_redirection() {
        if let Some(location) = resp_headers.get("location").and_then(|v| v.to_str().ok()) {
            if let Some(resolved) = resolve_redirect(location, &target_url) {
                info!("Redirect from {} to {}", target_url, resolved);
                return build_redirect_response(status, &resp_headers, &resolved);
            }
        }
    }

    // Normal response
    build_stream_response(status, &resp_headers, response).await
}

fn build_redirect_response(
    status: StatusCode,
    resp_headers: &HeaderMap,
    new_location: &str,
) -> Response {
    let filtered = filter_response_headers(resp_headers);
    let mut builder = Response::builder().status(status);

    let hdrs = builder.headers_mut().unwrap();
    for (name, value) in cors_headers() {
        hdrs.insert(name, value);
    }
    for (name, value) in filtered.iter() {
        if name.as_str() != "location" {
            hdrs.append(name.clone(), value.clone());
        }
    }
    if let Ok(loc) = HeaderValue::from_str(new_location) {
        hdrs.insert("location", loc);
    }

    builder.body(Body::empty()).unwrap()
}

async fn build_stream_response(
    status: StatusCode,
    resp_headers: &HeaderMap,
    response: reqwest::Response,
) -> Response {
    let filtered = filter_response_headers(resp_headers);
    let mut builder = Response::builder().status(status);

    let hdrs = builder.headers_mut().unwrap();
    for (name, value) in cors_headers() {
        hdrs.insert(name, value);
    }
    for (name, value) in filtered.iter() {
        hdrs.append(name.clone(), value.clone());
    }

    let stream = response.bytes_stream();
    let body = Body::from_stream(stream);

    builder.body(body).unwrap()
}

/// Convert axum HeaderMap to reqwest HeaderMap.
fn reqwest_headers(headers: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in headers.iter() {
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.append(n, v);
        }
    }
    out
}
