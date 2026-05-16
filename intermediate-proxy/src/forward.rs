use axum::http::{HeaderMap, HeaderValue, Method};
use base64::Engine;
use bytes::Bytes;
use std::time::Duration;

use crate::upstream::{Upstream, UpstreamKind};

/// Walk the `source()` chain of a reqwest error into a single dense string.
/// reqwest's top-level Display is just "error sending request for url (...)";
/// the real cause (Connection refused / dns error / handshake failure / reset)
/// lives deeper in the chain.
pub fn describe_reqwest_error(err: &reqwest::Error) -> String {
    use std::error::Error;
    let mut parts: Vec<String> = Vec::new();
    let mut current: Option<&dyn Error> = Some(err as &dyn Error);
    while let Some(e) = current {
        let s = e.to_string();
        if !parts.iter().any(|p| p == &s) {
            parts.push(s);
        }
        current = e.source();
    }
    if parts.len() > 1 {
        parts.remove(0);
    }
    parts.join(" → ")
}

/// Dispatch a single upstream request based on the upstream kind.
pub async fn send_to_upstream(
    upstream: &Upstream,
    method: &Method,
    headers: &HeaderMap,
    body: Bytes,
    extra_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, reqwest::Error> {
    match &upstream.kind {
        UpstreamKind::Endpoint { url, client } => {
            crate::upstream::endpoint::forward(client, url, method, headers, body, extra_headers)
                .await
        }
        UpstreamKind::HttpProxy { client } => {
            let target_url = proxy_common::target::decode_target(
                headers
                    .get("x-target")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
            )
            .unwrap_or_default();
            crate::upstream::http_proxy::forward(
                client,
                &target_url,
                method,
                headers,
                body,
                extra_headers,
            )
            .await
        }
        UpstreamKind::Socks5 { client } => {
            let target_url = proxy_common::target::decode_target(
                headers
                    .get("x-target")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
            )
            .unwrap_or_default();
            crate::upstream::socks::forward(
                client,
                &target_url,
                method,
                headers,
                body,
                extra_headers,
            )
            .await
        }
    }
}

pub struct ProbeReply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body_preview: Option<String>,
}

/// Issue a synthetic GET to `probe_url` through `upstream`, reading at most
/// `capture_bytes` of the body for the classifier.
pub async fn probe_request(
    upstream: &Upstream,
    probe_url: &str,
    timeout: Duration,
    capture_bytes: usize,
) -> Result<ProbeReply, String> {
    let mut headers = HeaderMap::new();
    let encoded = base64::engine::general_purpose::STANDARD.encode(probe_url.as_bytes());
    if let Ok(v) = HeaderValue::from_str(&encoded) {
        headers.insert("x-target", v);
    }
    headers.insert("accept", HeaderValue::from_static("*/*"));

    let resp = match tokio::time::timeout(
        timeout,
        send_to_upstream(upstream, &Method::GET, &headers, Bytes::new(), None),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(describe_reqwest_error(&e)),
        Err(_) => return Err(format!("timeout {}ms", timeout.as_millis())),
    };

    let status = resp.status().as_u16();
    let mut hdrs = Vec::new();
    for (k, v) in resp.headers().iter() {
        if let Ok(s) = v.to_str() {
            hdrs.push((k.as_str().to_ascii_lowercase(), s.to_string()));
        }
    }

    let body_preview = if capture_bytes > 0 {
        match tokio::time::timeout(timeout, resp.bytes()).await {
            Ok(Ok(b)) => {
                let take = b.len().min(capture_bytes);
                Some(String::from_utf8_lossy(&b[..take]).into_owned())
            }
            _ => None,
        }
    } else {
        None
    };

    Ok(ProbeReply {
        status,
        headers: hdrs,
        body_preview,
    })
}
