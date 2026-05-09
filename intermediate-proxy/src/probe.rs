use axum::http::{HeaderMap, HeaderValue, Method};
use base64::Engine;
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::queue::{is_fatal_reason, sanitize_url, ProxyQueue};
use crate::stream::{describe_reqwest_error, send_to_upstream};
use crate::upstream::Upstream;

pub async fn run_fatal_prober(queue: Arc<ProxyQueue>) {
    let probe_url = queue.fatal_probe_url().to_string();
    let interval = queue.fatal_probe_interval();
    let timeout = queue.upstream_timeout();

    if probe_url.is_empty() || interval.is_zero() {
        info!("Fatal prober disabled (FATAL_PROBE_URL or FATAL_PROBE_INTERVAL_MS unset)");
        return;
    }

    info!(
        "Fatal prober enabled: every {}ms against {}",
        interval.as_millis(),
        probe_url
    );

    loop {
        tokio::time::sleep(interval).await;
        let fatals = queue.fatal_regulars();
        if fatals.is_empty() {
            continue;
        }
        for entry in fatals {
            match probe_once(&entry.upstream, &probe_url, timeout).await {
                Ok(status) => {
                    info!(
                        "fatal prober: {} responded {} → restoring to pool",
                        sanitize_url(&entry.url),
                        status
                    );
                    queue.clear_entry_fatal(&entry);
                    entry.set_last_error_reason(&format!("recovered via probe ({status})"));
                }
                Err(reason) => {
                    if is_fatal_reason(&reason) {
                        debug!(
                            "fatal prober: {} still down: {}",
                            sanitize_url(&entry.url),
                            reason
                        );
                    } else {
                        warn!(
                            "fatal prober: {} non-connect error, restoring: {}",
                            sanitize_url(&entry.url),
                            reason
                        );
                        queue.clear_entry_fatal(&entry);
                    }
                    entry.set_last_error_reason(&reason);
                }
            }
        }
    }
}

async fn probe_once(
    upstream: &Upstream,
    probe_url: &str,
    timeout: Duration,
) -> Result<u16, String> {
    let mut headers = HeaderMap::new();
    let encoded = base64::engine::general_purpose::STANDARD.encode(probe_url.as_bytes());
    if let Ok(v) = HeaderValue::from_str(&encoded) {
        headers.insert("x-target", v);
    }
    headers.insert("accept", HeaderValue::from_static("*/*"));

    match tokio::time::timeout(
        timeout,
        send_to_upstream(upstream, &Method::GET, &headers, Bytes::new(), None),
    )
    .await
    {
        Ok(Ok(resp)) => Ok(resp.status().as_u16()),
        Ok(Err(e)) => Err(describe_reqwest_error(&e)),
        Err(_) => Err(format!("timeout {}ms", timeout.as_millis())),
    }
}
