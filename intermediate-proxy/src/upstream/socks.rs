use axum::http::{HeaderMap, Method};
use bytes::Bytes;
use proxy_common::headers::filter_request_headers;
use reqwest::Client;
use url::Url;

/// Forward a request through a SOCKS5 proxy.
/// The request goes directly to the target URL via the SOCKS5 proxy.
pub async fn forward(
    client: &Client,
    target_url: &str,
    method: &Method,
    headers: &HeaderMap,
    body: Bytes,
    extra_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, reqwest::Error> {
    let host = Url::parse(target_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();

    let mut filtered = filter_request_headers(headers, &host);

    if let Some(extra) = extra_headers {
        for (name, value) in extra.iter() {
            filtered.insert(name.clone(), value.clone());
        }
    }

    let mut req_headers = reqwest::header::HeaderMap::new();
    for (name, value) in filtered.iter() {
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            req_headers.append(n, v);
        }
    }

    let mut req_builder = client
        .request(method.clone(), target_url)
        .headers(req_headers);

    if *method != Method::GET && *method != Method::HEAD && !body.is_empty() {
        req_builder = req_builder.body(body);
    }

    req_builder.send().await
}
