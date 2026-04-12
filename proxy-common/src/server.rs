use std::net::SocketAddr;

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

/// Initialize tracing with the given default level (e.g. "info").
pub fn init_tracing(default_level: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level)),
        )
        .init();
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
