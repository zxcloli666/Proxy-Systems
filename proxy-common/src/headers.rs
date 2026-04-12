use axum::http::HeaderMap;

/// Request headers to strip when forwarding to the target.
const SKIP_REQUEST_HEADERS: &[&str] = &[
    "x-target",
    "host",
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
