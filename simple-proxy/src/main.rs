mod handler;
mod outbound;
mod redirect;

use axum::routing::any;
use axum::Router;
use proxy_common::cors::cors_layer;
use proxy_common::server::{bind_tcp, init_tracing, port_from_env};
use tracing::info;

#[tokio::main]
async fn main() {
    init_tracing("info");

    let port = port_from_env(8080);

    let profile = std::env::var("IMPERSONATE").ok();
    let outbound = match outbound::Outbound::new(profile.as_deref()) {
        Ok(outbound) => outbound,
        Err(e) => {
            tracing::error!("{e}");
            std::process::exit(1);
        }
    };
    let profile = outbound.profile().to_string();

    let auth_token = std::env::var("AUTH_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .map(std::sync::Arc::<str>::from);
    let auth_enabled = auth_token.is_some();

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
        "Simple Proxy running on http://0.0.0.0:{port} (auth required: {auth_enabled}, impersonate: {profile})"
    );

    axum::serve(listener, app).await.expect("server error");
}
