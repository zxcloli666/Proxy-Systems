use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Connect to a destination through a SOCKS5 proxy (no auth).
/// Returns the raw TCP stream after the SOCKS5 handshake is complete.
pub async fn socks5_connect(
    proxy_host: &str,
    proxy_port: u16,
    dest_host: &str,
    dest_port: u16,
    connect_timeout: Duration,
) -> std::io::Result<TcpStream> {
    let addr = format!("{proxy_host}:{proxy_port}");
    let mut stream = timeout(connect_timeout, TcpStream::connect(&addr))
        .await
        .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "SOCKS5 connect timeout")
        })??;

    // Greeting: VER=5, NMETHODS=1, METHOD=0 (no auth)
    stream.write_all(&[0x05, 0x01, 0x00]).await?;

    let mut greet = [0u8; 2];
    stream.read_exact(&mut greet).await?;
    if greet[0] != 0x05 || greet[1] != 0x00 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "SOCKS5 handshake rejected",
        ));
    }

    // CONNECT request: VER=5, CMD=1 (CONNECT), RSV=0, ATYP=3 (domain)
    let host_bytes = dest_host.as_bytes();
    let mut buf = Vec::with_capacity(5 + host_bytes.len() + 2);
    buf.push(0x05); // VER
    buf.push(0x01); // CMD CONNECT
    buf.push(0x00); // RSV
    buf.push(0x03); // ATYP domain
    buf.push(host_bytes.len() as u8);
    buf.extend_from_slice(host_bytes);
    buf.push((dest_port >> 8) as u8);
    buf.push(dest_port as u8);
    stream.write_all(&buf).await?;

    // Read response (at least 10 bytes for IPv4, but we read minimum needed)
    let mut resp = [0u8; 10];
    stream.read_exact(&mut resp).await?;
    if resp[0] != 0x05 || resp[1] != 0x00 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!("SOCKS5 connect error code={}", resp[1]),
        ));
    }

    // If ATYP is domain (3) or IPv6 (4), we need to read additional bytes
    match resp[3] {
        0x01 => {} // IPv4: we already read all 10 bytes (4 + 2)
        0x03 => {
            // Domain: need to read domain_len + domain + port - already read 4 of the 10
            // resp[4] is domain length, we need to skip domain_len + 2 - 6 more bytes
            let domain_len = resp[4] as usize;
            let extra = domain_len + 2 - 5; // total needed minus what we already have in resp[5..10]
            if extra > 0 {
                let mut skip = vec![0u8; extra];
                stream.read_exact(&mut skip).await?;
            }
        }
        0x04 => {
            // IPv6: 16 + 2 - 6 = 12 extra bytes
            let mut skip = [0u8; 12];
            stream.read_exact(&mut skip).await?;
        }
        _ => {}
    }

    Ok(stream)
}
