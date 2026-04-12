use axum::http::{HeaderMap, Method};
use bytes::Bytes;
use reqwest::Client;

/// Forward a request to an endpoint proxy that uses X-Target header.
/// The endpoint receives all original headers plus X-Target with the base64 target URL.
pub async fn forward(
    client: &Client,
    endpoint_url: &str,
    method: &Method,
    headers: &HeaderMap,
    body: Bytes,
    extra_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut req_headers = reqwest::header::HeaderMap::new();

    // Copy all original headers (including x-target, host, etc.)
    for (name, value) in headers.iter() {
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            req_headers.append(n, v);
        }
    }

    // Apply extra headers (e.g., Range for recovery)
    if let Some(extra) = extra_headers {
        for (name, value) in extra.iter() {
            if let (Ok(n), Ok(v)) = (
                reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
                reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
            ) {
                req_headers.insert(n, v);
            }
        }
    }

    let mut req_builder = client
        .request(method.clone(), endpoint_url)
        .headers(req_headers);

    if *method != Method::GET && *method != Method::HEAD && !body.is_empty() {
        req_builder = req_builder.body(body);
    }

    req_builder.send().await
}
