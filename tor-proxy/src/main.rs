mod control;
mod handler;
mod health;
mod pool;
mod socks5;
mod tunnel;

use axum::routing::{any, get};
use axum::Router;
use handler::HandlerState;
use pool::{NodePool, TorNodeConfig};
use proxy_common::cors::cors_layer;
use proxy_common::server::{bind_tcp, init_tracing, port_from_env};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

#[tokio::main]
async fn main() {
    init_tracing("info");

    let port = port_from_env(8080);

    let tor_nodes: Vec<TorNodeConfig> = std::env::var("TOR_NODES")
        .unwrap_or_else(|_| "tor-node-1:9050:9051".to_string())
        .split(',')
        .filter_map(|entry| {
            let parts: Vec<&str> = entry.trim().split(':').collect();
            if parts.len() == 3 {
                Some(TorNodeConfig {
                    host: parts[0].to_string(),
                    socks_port: parts[1].parse().ok()?,
                    control_port: parts[2].parse().ok()?,
                })
            } else {
                None
            }
        })
        .collect();

    let password =
        std::env::var("TOR_CONTROL_PASSWORD").unwrap_or_else(|_| "torcontrol".to_string());
    let rotation_interval = std::env::var("ROTATION_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(3_600_000);
    let error_threshold = std::env::var("ERROR_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(3);
    let newnym_cooldown_ms = std::env::var("NEWNYM_COOLDOWN_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(15_000);
    let socks_timeout_ms = std::env::var("SOCKS_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(15_000);
    let request_timeout_ms = std::env::var("REQUEST_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30_000);

    info!(
        "Tor nodes: {}",
        tor_nodes
            .iter()
            .map(|n| n.host.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    info!(
        "Rotation: every {}s | threshold: {} errors",
        rotation_interval / 1000,
        error_threshold
    );
    info!(
        "NEWNYM cooldown: {}s | SOCKS timeout: {}s",
        newnym_cooldown_ms / 1000,
        socks_timeout_ms / 1000
    );

    let pool = NodePool::new(
        tor_nodes,
        password,
        error_threshold,
        Duration::from_millis(newnym_cooldown_ms),
        Duration::from_millis(rotation_interval),
    );

    let state = HandlerState {
        pool: Arc::clone(&pool),
        socks_timeout: Duration::from_millis(socks_timeout_ms),
        request_timeout: Duration::from_millis(request_timeout_ms),
    };

    let app = Router::new()
        .route("/health", get(health::health_handler))
        .route("/{*path}", any(handler::proxy_handler))
        .route("/", any(handler::proxy_handler))
        .layer(cors_layer())
        .with_state(state.clone());

    let listener = bind_tcp(port).await;
    info!("Tor Proxy running on http://0.0.0.0:{port}");

    axum::serve(listener, app).await.expect("server error");
}
