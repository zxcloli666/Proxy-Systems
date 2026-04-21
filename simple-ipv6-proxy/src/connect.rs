use std::io;
use std::net::{SocketAddr, SocketAddrV6};
use std::time::Duration;

use tokio::net::{lookup_host, TcpSocket, TcpStream};

use crate::subnet::Subnet;

/// Resolve a host to its IPv6 addresses only (AAAA records). A records are
/// dropped. If no IPv6 address is returned, the result is empty.
pub async fn resolve_ipv6(host: &str, port: u16) -> io::Result<Vec<SocketAddrV6>> {
    let iter = lookup_host((host, port)).await?;
    Ok(iter
        .filter_map(|sa| match sa {
            SocketAddr::V6(v6) => Some(v6),
            SocketAddr::V4(_) => None,
        })
        .collect())
}

/// Create a TCP socket bound to a random address within `subnet` (if any) and
/// connect to `target`. IPV6_FREEBIND is set on Linux so the kernel accepts
/// binding to any address in the routed prefix, even if it isn't assigned to
/// a local interface.
pub async fn connect_ipv6(
    target: SocketAddrV6,
    subnet: Option<&Subnet>,
    timeout: Duration,
) -> io::Result<TcpStream> {
    let socket = TcpSocket::new_v6()?;
    socket.set_nodelay(true)?;

    set_freebind(&socket);

    if let Some(s) = subnet {
        let src = s.random_addr();
        let bind_addr = SocketAddrV6::new(src, 0, 0, 0);
        socket.bind(SocketAddr::V6(bind_addr))?;
        tracing::debug!("bound source {} -> {}", src, target.ip());
    }

    match tokio::time::timeout(timeout, socket.connect(SocketAddr::V6(target))).await {
        Ok(res) => res,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "connect timeout")),
    }
}

#[cfg(target_os = "linux")]
fn set_freebind(socket: &TcpSocket) {
    use std::os::fd::AsRawFd;
    // IPV6_FREEBIND = 78 on Linux; not always exposed by libc crate constants.
    const IPV6_FREEBIND: libc::c_int = 78;
    let fd = socket.as_raw_fd();
    let one: libc::c_int = 1;
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_IPV6,
            IPV6_FREEBIND,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        tracing::debug!(
            "IPV6_FREEBIND setsockopt failed: {}",
            io::Error::last_os_error()
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn set_freebind(_socket: &TcpSocket) {}
