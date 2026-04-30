mod handler;
mod health;
mod queue;
mod stream;
mod tls;
mod upstream;

use axum::routing::{any, get};
use axum::Router;
use proxy_common::cors::cors_layer;
use proxy_common::server::{bind_tcp, init_tracing, port_from_env};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use crate::queue::{
    DEFAULT_HEDGE_DELAY_MS, DEFAULT_MAX_PARALLEL_HEDGE, DEFAULT_RESORT_INTERVAL_MS,
    DEFAULT_SLOW_THRESHOLD_MS, DEFAULT_UPSTREAM_TIMEOUT_MS,
};

#[tokio::main]
async fn main() {
    init_tracing("info");

    let regular_urls = parse_urls_env("PROXY_URL", "http://localhost:8080");
    let reserve_urls = parse_urls_env("RESERVE_PROXY_URL", "");

    let slow_threshold_ms = env_u64("SLOW_THRESHOLD_MS", DEFAULT_SLOW_THRESHOLD_MS);
    let upstream_timeout_ms = env_u64("UPSTREAM_TIMEOUT_MS", DEFAULT_UPSTREAM_TIMEOUT_MS);
    let resort_interval_ms = env_u64("RESORT_INTERVAL_MS", DEFAULT_RESORT_INTERVAL_MS);
    let hedge_delay_ms = env_u64("HEDGE_DELAY_MS", DEFAULT_HEDGE_DELAY_MS);
    let max_parallel_hedge =
        env_u64("MAX_PARALLEL_HEDGE", DEFAULT_MAX_PARALLEL_HEDGE as u64) as usize;

    info!(
        "Configured: {} regular + {} reserve proxies; slow>{}ms, timeout={}ms, resort every {}ms, hedge delay={}ms / parallel={}",
        regular_urls.len(),
        reserve_urls.len(),
        slow_threshold_ms,
        upstream_timeout_ms,
        resort_interval_ms,
        hedge_delay_ms,
        max_parallel_hedge,
    );

    let queue = queue::ProxyQueue::new(
        &regular_urls,
        &reserve_urls,
        slow_threshold_ms,
        Duration::from_millis(upstream_timeout_ms),
        Duration::from_millis(hedge_delay_ms),
        max_parallel_hedge,
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

    if env_bool("TLS_ENABLED", false) {
        let domains_env = std::env::var("DOMAINS").unwrap_or_default();
        let domains = tls::parse_domains(&domains_env);
        if domains.is_empty() {
            panic!("TLS_ENABLED=true but DOMAINS is empty (expected comma-separated domain list)");
        }
        let email = std::env::var("ACME_EMAIL")
            .unwrap_or_else(|_| format!("admin@{}", domains[0]));
        let cache_dir = PathBuf::from(
            std::env::var("ACME_CACHE_DIR").unwrap_or_else(|_| "/var/cache/acme".to_string()),
        );
        let staging = env_bool("ACME_STAGING", false);

        info!(
            "TLS mode on: {} domain(s), cache={:?}, staging={}",
            domains.len(),
            cache_dir,
            staging
        );
        tls::serve(domains, email, cache_dir, staging, app).await;
    } else {
        let port = port_from_env(3000);
        let listener = bind_tcp(port).await;
        info!("Intermediate Proxy running on http://0.0.0.0:{port}");
        axum::serve(listener, app).await.expect("server error");
    }
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

fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}
