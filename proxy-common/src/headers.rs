use axum::http::{header::ACCEPT_ENCODING, HeaderMap, HeaderValue};

/// Request headers to strip when forwarding to the target.
const SKIP_REQUEST_HEADERS: &[&str] = &[
    "x-target",
    "host",
    // We force `accept-encoding: identity` below: the proxy streams the body
    // through and strips `content-encoding` on the way back (see
    // SKIP_RESPONSE_HEADERS) WITHOUT decompressing, so a compressed upstream
    // body would arrive at the client undecodable. Dropping the client's value
    // here lets the forced one win regardless of what the client asked for.
    "accept-encoding",
    "cf-connecting-ip",
    "cf-ipcountry",
    "cf-ray",
    "cf-visitor",
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-real-ip",
];

/// Response headers to strip when sending back to the client.
const SKIP_RESPONSE_HEADERS: &[&str] = &[
    "content-security-policy",
    "x-frame-options",
    "content-encoding",
    "content-length",
    "transfer-encoding",
];

/// Filter request headers: remove proxy-specific headers and set the correct Host.
pub fn filter_request_headers(headers: &HeaderMap, target_host: &str) -> HeaderMap {
    let mut filtered = HeaderMap::new();
    for (name, value) in headers.iter() {
        let name_lower = name.as_str();
        if !SKIP_REQUEST_HEADERS.contains(&name_lower) {
            filtered.append(name.clone(), value.clone());
        }
    }
    if let Ok(host_val) = target_host.parse() {
        filtered.insert("host", host_val);
    }
    // Force an uncompressed upstream response. Since the body is forwarded
    // as-is and `content-encoding` is stripped from the response without
    // decompressing, asking the target for `identity` keeps request/response
    // framing consistent (otherwise gzip/br bodies reach the client as garbage).
    filtered.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    filtered
}

/// Filter response headers: remove problematic headers.
pub fn filter_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut filtered = HeaderMap::new();
    for (name, value) in headers.iter() {
        let name_lower = name.as_str();
        if !SKIP_RESPONSE_HEADERS.contains(&name_lower) {
            filtered.append(name.clone(), value.clone());
        }
    }
    filtered
}
