mod connect;
mod filter;
mod handler;
mod redirect;
mod subnet;

use axum::routing::any;
use axum::Router;
use proxy_common::cors::cors_layer;
use proxy_common::server::{bind_tcp, init_tracing, port_from_env};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use crate::handler::HandlerState;
use crate::subnet::Subnet;

#[tokio::main]
async fn main() {
    init_tracing("info");

    let _ = rustls::crypto::ring::default_provider().install_default();

    let port = port_from_env(8080);
    let connect_timeout = env_ms("CONNECT_TIMEOUT_MS", 5_000);
    let request_timeout = env_ms("REQUEST_TIMEOUT_MS", 30_000);
    let max_attempts = env_u32("MAX_ATTEMPTS", 3).max(1);
    let retry_codes = parse_retry_codes();
    let subnet = Subnet::from_env();

    match &subnet {
        Some(s) => info!(
            "IPv6 source subnet: {}/{} (random source per attempt)",
            s.prefix, s.prefix_len
        ),
        None => info!(
            "No IPV6_SUBNET configured; using default IPv6 source address"
        ),
    }
    info!(
        "Max attempts: {} | retry on statuses: {:?}",
        max_attempts, retry_codes
    );

    let state = HandlerState {
        subnet: Arc::new(subnet),
        connect_timeout,
        request_timeout,
        max_attempts,
        retry_codes: Arc::new(retry_codes),
    };

    let app = Router::new()
        .route("/{*path}", any(handler::proxy_handler))
        .route("/", any(handler::proxy_handler))
        .layer(cors_layer())
        .with_state(state);

    let listener = bind_tcp(port).await;
    info!("Simple IPv6 Proxy running on http://0.0.0.0:{port}");

    axum::serve(listener, app).await.expect("server error");
}

fn env_ms(key: &str, default_ms: u64) -> Duration {
    let ms = std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default_ms);
    Duration::from_millis(ms)
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn parse_retry_codes() -> Vec<u16> {
    const DEFAULT: &[u16] = &[403, 429];
    match std::env::var("RETRY_STATUS_CODES") {
        Ok(s) => {
            let parsed: Vec<u16> = s
                .split(',')
                .filter_map(|x| x.trim().parse().ok())
                .collect();
            if parsed.is_empty() {
                DEFAULT.to_vec()
            } else {
                parsed
            }
        }
        Err(_) => DEFAULT.to_vec(),
    }
}
