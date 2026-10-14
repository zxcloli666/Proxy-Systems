use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use axum::Router;
use axum_server::accept::{Accept, DefaultAcceptor};
use rustls_acme::caches::DirCache;
use rustls_acme::AcmeConfig;
use socket2::{SockRef, TcpKeepalive};
use tokio::net::TcpStream;
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
    let acceptor = Guarded(state.axum_acceptor(rustls_config));

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
            .acceptor(Guarded(DefaultAcceptor::new()))
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

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct Guarded<A>(A);

impl<A, S> Accept<TcpStream, S> for Guarded<A>
where
    A: Accept<TcpStream, S>,
    A::Future: Send + 'static,
{
    type Stream = A::Stream;
    type Service = A::Service;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let keepalive = TcpKeepalive::new()
            .with_time(Duration::from_secs(60))
            .with_interval(Duration::from_secs(10))
            .with_retries(3);
        let _ = SockRef::from(&stream).set_tcp_keepalive(&keepalive);
        let accepted = self.0.accept(stream, service);
        Box::pin(async move {
            tokio::time::timeout(HANDSHAKE_TIMEOUT, accepted)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "tls handshake timed out"))?
        })
    }
}
