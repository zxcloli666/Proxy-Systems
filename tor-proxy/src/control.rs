use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Send SIGNAL NEWNYM to a Tor control port to rotate the circuit.
pub async fn send_newnym(host: &str, port: u16, password: &str) -> std::io::Result<()> {
    let addr = format!("{host}:{port}");
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(&addr))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "control port timeout"))??;

    // Authenticate
    let auth_cmd = format!("AUTHENTICATE \"{password}\"\r\n");
    stream.write_all(auth_cmd.as_bytes()).await?;

    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    if !line.starts_with("250") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("auth failed: {}", line.trim()),
        ));
    }

    // Send NEWNYM
    let writer = reader.into_inner();
    writer.write_all(b"SIGNAL NEWNYM\r\n").await?;

    let mut reader = BufReader::new(writer);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    if !line.starts_with("250") {
        return Err(std::io::Error::other(format!(
            "NEWNYM failed: {}",
            line.trim()
        )));
    }

    Ok(())
}
