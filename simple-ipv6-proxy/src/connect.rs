use std::io;
use std::net::{SocketAddr, SocketAddrV6};

use tokio::net::lookup_host;

pub async fn resolve_ipv6(host: &str, port: u16) -> io::Result<Vec<SocketAddrV6>> {
    let iter = lookup_host((host, port)).await?;
    Ok(iter
        .filter_map(|sa| match sa {
            SocketAddr::V6(v6) => Some(v6),
            SocketAddr::V4(_) => None,
        })
        .collect())
}
