mod handler;
mod health;
mod queue;
mod stream;
mod upstream;

use axum::routing::{any, get};
use axum::Router;
use proxy_common::cors::cors_layer;
use proxy_common::server::{bind_tcp, init_tracing, port_from_env};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use crate::queue::{
    DEFAULT_RESORT_INTERVAL_MS, DEFAULT_SLOW_THRESHOLD_MS, DEFAULT_UPSTREAM_TIMEOUT_MS,
};

#[tokio::main]
async fn main() {
    init_tracing("info");

    let port = port_from_env(3000);

    let regular_urls = parse_urls_env("PROXY_URL", "http://localhost:8080");
    let reserve_urls = parse_urls_env("RESERVE_PROXY_URL", "");

    let slow_threshold_ms = env_u64("SLOW_THRESHOLD_MS", DEFAULT_SLOW_THRESHOLD_MS);
    let upstream_timeout_ms = env_u64("UPSTREAM_TIMEOUT_MS", DEFAULT_UPSTREAM_TIMEOUT_MS);
    let resort_interval_ms = env_u64("RESORT_INTERVAL_MS", DEFAULT_RESORT_INTERVAL_MS);

    info!(
        "Configured: {} regular + {} reserve proxies; slow>{}ms, timeout={}ms, resort every {}ms",
        regular_urls.len(),
        reserve_urls.len(),
        slow_threshold_ms,
        upstream_timeout_ms,
        resort_interval_ms
    );

    let queue = queue::ProxyQueue::new(
        &regular_urls,
        &reserve_urls,
        slow_threshold_ms,
        Duration::from_millis(upstream_timeout_ms),
    );

    tokio::spawn(
        Arc::clone(&queue).run_resorter(Duration::from_millis(resort_interval_ms)),
    );

    let app = Router::new()
        .route("/health", get(health::health_handler))
        .route("/{*path}", any(handler::proxy_handler))
        .route("/", any(handler::proxy_handler))
        .layer(cors_layer())
        .with_state(Arc::clone(&queue));

    let listener = bind_tcp(port).await;
    info!("Intermediate Proxy running on http://0.0.0.0:{port}");

    axum::serve(listener, app).await.expect("server error");
}

fn parse_urls_env(key: &str, default: &str) -> Vec<String> {
    std::env::var(key)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
