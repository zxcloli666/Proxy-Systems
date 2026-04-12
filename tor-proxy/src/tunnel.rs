use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper::Request;
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use std::time::Duration;
use tokio_rustls::TlsConnector;
use url::Url;

use crate::socks5::socks5_connect;

/// Perform an HTTP request over a SOCKS5 tunnel (with optional TLS upgrade).
#[allow(clippy::too_many_arguments)]
pub async fn http_over_socks5(
    proxy_host: &str,
    proxy_port: u16,
    target_url: &str,
    method: &str,
    headers: Vec<(String, String)>,
    body: Option<Bytes>,
    socks_timeout: Duration,
    request_timeout: Duration,
) -> Result<(u16, Vec<(String, String)>, Bytes), Box<dyn std::error::Error + Send + Sync>> {
    let parsed = Url::parse(target_url)?;
    let host = parsed.host_str().ok_or("no host in URL")?.to_string();
    let is_https = parsed.scheme() == "https";
    let port = parsed.port().unwrap_or(if is_https { 443 } else { 80 });

    // SOCKS5 connect
    let raw_stream = socks5_connect(proxy_host, proxy_port, &host, port, socks_timeout).await?;

    let path = if parsed.query().is_some() {
        format!("{}?{}", parsed.path(), parsed.query().unwrap())
    } else {
        parsed.path().to_string()
    };

    if is_https {
        // TLS upgrade
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();

        let connector = TlsConnector::from(Arc::new(config));
        let server_name = rustls::pki_types::ServerName::try_from(host.clone())?;
        let tls_stream = connector.connect(server_name, raw_stream).await?;

        let io = TokioIo::new(tls_stream);
        send_http1_request(io, &host, &path, method, headers, body, request_timeout).await
    } else {
        let io = TokioIo::new(raw_stream);
        send_http1_request(io, &host, &path, method, headers, body, request_timeout).await
    }
}

async fn send_http1_request<S>(
    io: TokioIo<S>,
    host: &str,
    path: &str,
    method: &str,
    headers: Vec<(String, String)>,
    body: Option<Bytes>,
    request_timeout: Duration,
) -> Result<(u16, Vec<(String, String)>, Bytes), Box<dyn std::error::Error + Send + Sync>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = http1::handshake(io).await?;

    // Spawn connection driver
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::error!("Connection error: {}", e);
        }
    });

    // Build request
    let http_method: hyper::Method = method.parse()?;

    let mut req_builder = Request::builder()
        .method(http_method)
        .uri(path)
        .header("host", host);

    for (name, value) in &headers {
        req_builder = req_builder.header(name.as_str(), value.as_str());
    }

    let response = if let Some(body_bytes) = body {
        let req = req_builder.body(Full::new(body_bytes))?;
        tokio::time::timeout(request_timeout, sender.send_request(req)).await??
    } else {
        // We need a common body type — use Full with empty bytes
        let req = req_builder.body(Full::new(Bytes::new()))?;
        tokio::time::timeout(request_timeout, sender.send_request(req)).await??
    };

    let status = response.status().as_u16();

    let resp_headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    let body_bytes = response.into_body().collect().await?.to_bytes();

    Ok((status, resp_headers, body_bytes))
}
