use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use rustls_acme::caches::DirCache;
use rustls_acme::AcmeConfig;
use tokio_stream::StreamExt;
use tracing::{error, info, warn};

pub fn parse_domains(env_value: &str) -> Vec<String> {
    env_value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub async fn serve(
    domains: Vec<String>,
    email: String,
    cache_dir: PathBuf,
    staging: bool,
    app: Router,
) {
    let _ = rustls::crypto::ring::default_provider().install_default();

    if let Err(e) = tokio::fs::create_dir_all(&cache_dir).await {
        warn!("failed to create ACME cache dir {:?}: {}", cache_dir, e);
    }

    let mut state = AcmeConfig::new(domains.clone())
        .contact_push(format!("mailto:{email}"))
        .cache(DirCache::new(cache_dir))
        .directory_lets_encrypt(!staging)
        .state();

    let rustls_config = state.default_rustls_config();
    let acceptor = state.axum_acceptor(rustls_config);

    tokio::spawn(async move {
        while let Some(res) = state.next().await {
            match res {
                Ok(ok) => info!("acme event: {:?}", ok),
                Err(err) => error!("acme error: {:?}", err),
            }
        }
    });

    let https_addr = SocketAddr::from(([0, 0, 0, 0], 443));
    let http_addr = SocketAddr::from(([0, 0, 0, 0], 80));

    info!(
        "TLS mode: {} domain(s), :443 HTTPS + :80 HTTP (staging={})",
        domains.len(),
        staging
    );

    let http_app = app.clone();
    let http_task = tokio::spawn(async move {
        if let Err(e) = axum_server::bind(http_addr)
            .serve(http_app.into_make_service())
            .await
        {
            error!("HTTP :80 server error: {}", e);
        }
    });

    let https_task = tokio::spawn(async move {
        if let Err(e) = axum_server::bind(https_addr)
            .acceptor(acceptor)
            .serve(app.into_make_service())
            .await
        {
            error!("HTTPS :443 server error: {}", e);
        }
    });

    let _ = tokio::join!(http_task, https_task);
}
