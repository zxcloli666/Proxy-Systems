use axum::http::{HeaderMap, Method};
use bytes::Bytes;
use reqwest::Client;

/// Hop-by-hop + connection-scoped headers we must NOT forward to the
/// upstream worker endpoint. In particular, leaking the client's `host`
/// causes Cloudflare workers.dev to reject the request with 421
/// ("Misdirected Request") because the SNI doesn't match the Host header.
const SKIP_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
    "cf-connecting-ip",
    "cf-ipcountry",
    "cf-ray",
    "cf-visitor",
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
    "x-real-ip",
];

/// Forward a request to an endpoint proxy that uses the X-Target header.
/// The endpoint receives the original client headers (minus hop-by-hop and
/// host-scoped headers) plus X-Target with the base64 target URL. Reqwest
/// fills in Host from the endpoint URL automatically.
pub async fn forward(
    client: &Client,
    endpoint_url: &str,
    method: &Method,
    headers: &HeaderMap,
    body: Bytes,
    extra_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut req_headers = reqwest::header::HeaderMap::new();

    for (name, value) in headers.iter() {
        if SKIP_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            req_headers.append(n, v);
        }
    }

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
