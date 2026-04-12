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
use tracing::info;

#[tokio::main]
async fn main() {
    init_tracing("info");

    let port = port_from_env(3000);

    let regular_urls: Vec<String> = std::env::var("PROXY_URL")
        .unwrap_or_else(|_| "http://localhost:8080".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let reserve_urls: Vec<String> = std::env::var("RESERVE_PROXY_URL")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    info!(
        "Configured proxy servers: {} regular + {} reserve",
        regular_urls.len(),
        reserve_urls.len()
    );

    let queue = queue::ProxyQueue::new(&regular_urls, &reserve_urls);

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
