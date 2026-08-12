mod handler;
mod outbound;
mod redirect;

use axum::routing::any;
use axum::Router;
use proxy_common::cors::cors_layer;
use proxy_common::server::{bind_tcp, init_tracing, port_from_env};
use reqwest::Client;
use tracing::info;

#[tokio::main]
async fn main() {
    init_tracing("info");

    let port = port_from_env(8080);

    let client = Client::builder()
        .http1_only()
        .pool_max_idle_per_host(128)
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .tcp_nodelay(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build HTTP client");

    let auth_token = std::env::var("AUTH_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .map(std::sync::Arc::<str>::from);
    let auth_enabled = auth_token.is_some();

    let impersonate = std::env::var("IMPERSONATE").ok().filter(|p| !p.is_empty());
    let outbound = match &impersonate {
        Some(profile) => match outbound::Outbound::emulated(profile) {
            Ok(client) => client,
            Err(e) => {
                tracing::error!("{e}");
                std::process::exit(1);
            }
        },
        None => outbound::Outbound::plain(client),
    };

    let app = Router::new()
        .route("/{*path}", any(handler::proxy_handler))
        .route("/", any(handler::proxy_handler))
        .layer(cors_layer())
        .with_state(handler::AppState {
            client: std::sync::Arc::new(outbound),
            auth_token,
        });

    let listener = bind_tcp(port).await;
    info!(
        "Simple Proxy running on http://0.0.0.0:{port} (auth required: {auth_enabled}, impersonate: {})",
        impersonate.as_deref().unwrap_or("off")
    );

    axum::serve(listener, app).await.expect("server error");
}
