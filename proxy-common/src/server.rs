use std::net::SocketAddr;

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

/// Initialize tracing. Resolution order:
///   1. `RUST_LOG` (full EnvFilter directive, e.g. `intermediate_proxy=debug,warn`)
///   2. `LOG_LEVEL` (simple level name: trace/debug/info/warn/error)
///   3. the provided `default_level`
pub fn init_tracing(default_level: &str) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| {
            let level = std::env::var("LOG_LEVEL")
                .ok()
                .unwrap_or_else(|| default_level.to_string());
            EnvFilter::try_new(level)
        })
        .unwrap_or_else(|_| EnvFilter::new(default_level));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Create a TCP listener with SO_REUSEADDR on the given port.
pub async fn bind_tcp(port: u16) -> TcpListener {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let socket = tokio::net::TcpSocket::new_v4().expect("failed to create socket");
    socket
        .set_reuseaddr(true)
        .expect("failed to set SO_REUSEADDR");

    #[cfg(target_os = "linux")]
    socket
        .set_reuseport(true)
        .expect("failed to set SO_REUSEPORT");

    socket.bind(addr).expect("failed to bind");
    socket.listen(1024).expect("failed to listen")
}

/// Read port from `PORT` env var, defaulting to the given value.
pub fn port_from_env(default: u16) -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
