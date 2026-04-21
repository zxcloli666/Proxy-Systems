use std::net::{IpAddr, SocketAddrV6};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyStream, Full};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use proxy_common::cors::cors_headers;
use proxy_common::headers::{filter_request_headers, filter_response_headers};
use proxy_common::response::text_response;
use proxy_common::target::decode_target;
use tokio_rustls::TlsConnector;
use tracing::{debug, error, info, warn};
use url::Url;

use crate::connect::{connect_ipv6, resolve_ipv6};
use crate::filter;
use crate::redirect::resolve_redirect;
use crate::subnet::Subnet;

const MAX_REQUEST_BODY: usize = 10 * 1024 * 1024;

#[derive(Clone)]
pub struct HandlerState {
    pub subnet: Arc<Option<Subnet>>,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_attempts: u32,
    pub retry_codes: Arc<Vec<u16>>,
}

pub async fn proxy_handler(State(state): State<HandlerState>, req: Request<Body>) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();

    let target_header = match headers.get("x-target").and_then(|v| v.to_str().ok()) {
        Some(v) => v.to_string(),
        None => return text_response(StatusCode::BAD_REQUEST, "Missing X-Target header"),
    };

    let target_url = match decode_target(&target_header) {
        Some(u) => u,
        None => return text_response(StatusCode::BAD_REQUEST, "Invalid base64 encoded URL"),
    };

    let parsed = match Url::parse(&target_url) {
        Ok(u) => u,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Invalid target URL"),
    };

    let host = match parsed.host_str() {
        Some(h) => h.to_string(),
        None => return text_response(StatusCode::BAD_REQUEST, "Target URL has no host"),
    };
    let is_https = parsed.scheme() == "https";
    let port = parsed.port().unwrap_or(if is_https { 443 } else { 80 });

    // SSRF guard: reject literal internal/loopback IPs up front. Also rejects
    // IPv4 literals outright (this proxy is v6-only).
    if let Ok(literal) = host.parse::<IpAddr>() {
        if filter::is_blocked_ip(literal) {
            warn!("blocked target literal IP: {}", literal);
            return text_response(
                StatusCode::BAD_REQUEST,
                "target address is internal/reserved and not allowed",
            );
        }
        if literal.is_ipv4() {
            return text_response(
                StatusCode::BAD_GATEWAY,
                "IPv4 literal targets are not supported by this proxy",
            );
        }
    }
    if is_blocked_hostname(&host) {
        warn!("blocked target hostname: {}", host);
        return text_response(
            StatusCode::BAD_REQUEST,
            "target hostname is internal and not allowed",
        );
    }

    info!("{} {}", method, target_url);

    let resolved = match resolve_ipv6(&host, port).await {
        Ok(v) => v,
        Err(e) => {
            warn!("DNS lookup failed for {}: {}", host, e);
            return text_response(
                StatusCode::BAD_GATEWAY,
                &format!("IPv6 DNS lookup failed: {e}"),
            );
        }
    };

    let v6_addrs: Vec<SocketAddrV6> = resolved
        .into_iter()
        .filter(|sa| !filter::is_blocked_ip(IpAddr::V6(*sa.ip())))
        .collect();

    if v6_addrs.is_empty() {
        warn!("No public IPv6 (AAAA) records for {}", host);
        return text_response(
            StatusCode::BAD_GATEWAY,
            &format!("No public IPv6 address available for {host}"),
        );
    }

    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_REQUEST_BODY).await {
        Ok(b) => b,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Failed to read request body"),
    };

    let filtered = filter_request_headers(&headers, &host);
    let req_body = if method != Method::GET && method != Method::HEAD && !body_bytes.is_empty() {
        body_bytes
    } else {
        Bytes::new()
    };

    let path_and_query = match parsed.query() {
        Some(q) => format!("{}?{q}", parsed.path()),
        None => parsed.path().to_string(),
    };

    let max_attempts = state.max_attempts.max(1) as usize;
    let mut last_err: Option<String> = None;

    for attempt in 0..max_attempts {
        let target = v6_addrs[attempt % v6_addrs.len()];
        match dispatch(
            &state,
            target,
            is_https,
            &host,
            &path_and_query,
            &method,
            &filtered,
            req_body.clone(),
        )
        .await
        {
            Ok((status, resp_headers, body)) => {
                let code = status.as_u16();
                let is_last = attempt + 1 >= max_attempts;
                if !is_last && state.retry_codes.contains(&code) {
                    // Target rejected this source IP (likely ban / rate-limit).
                    // Drop the body so the upstream conn tears down, retry with
                    // a fresh random source on the next iteration.
                    warn!(
                        "attempt {}/{} via {} -> {} (retrying with new source)",
                        attempt + 1,
                        max_attempts,
                        target.ip(),
                        status
                    );
                    drop(body);
                    last_err = Some(format!("status {status} from {}", target.ip()));
                    continue;
                }

                if status.is_redirection() {
                    if let Some(loc) = resp_headers.get("location").and_then(|v| v.to_str().ok()) {
                        if let Some(resolved) = resolve_redirect(loc, &target_url) {
                            info!("redirect {} -> {}", target_url, resolved);
                            return build_redirect_response(status, &resp_headers, &resolved);
                        }
                    }
                }
                info!(
                    "attempt {}/{} via {} -> {}",
                    attempt + 1,
                    max_attempts,
                    target.ip(),
                    status
                );
                return build_stream_response(status, &resp_headers, body);
            }
            Err(e) => {
                warn!(
                    "attempt {}/{} via {} failed: {}",
                    attempt + 1,
                    max_attempts,
                    target.ip(),
                    e
                );
                last_err = Some(e.to_string());
                continue;
            }
        }
    }

    error!("All {} attempts failed for {}", max_attempts, host);
    text_response(
        StatusCode::BAD_GATEWAY,
        &format!(
            "IPv6 connection failed after {} attempts: {}",
            max_attempts,
            last_err.as_deref().unwrap_or("unknown error")
        ),
    )
}

/// Block a handful of hostnames that resolvers will happily turn into
/// loopback. The resolved-IP filter already catches these, but blocking them
/// by name gives a clearer error and avoids a DNS round-trip.
fn is_blocked_hostname(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "ip6-localhost" | "ip6-loopback" | "localhost.localdomain"
    )
}

type UpstreamResponse = (StatusCode, HeaderMap, hyper::body::Incoming);

async fn dispatch(
    state: &HandlerState,
    target: SocketAddrV6,
    is_https: bool,
    host: &str,
    path_and_query: &str,
    method: &Method,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<UpstreamResponse, Box<dyn std::error::Error + Send + Sync>> {
    let subnet_ref = state.subnet.as_ref().as_ref();
    let stream = connect_ipv6(target, subnet_ref, state.connect_timeout).await?;

    debug!("connected to {}", target);

    if is_https {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let server_name = rustls::pki_types::ServerName::try_from(host.to_string())?;
        let tls_stream = connector.connect(server_name, stream).await?;
        send_request(
            TokioIo::new(tls_stream),
            host,
            path_and_query,
            method,
            headers,
            body,
            state.request_timeout,
        )
        .await
    } else {
        send_request(
            TokioIo::new(stream),
            host,
            path_and_query,
            method,
            headers,
            body,
            state.request_timeout,
        )
        .await
    }
}

async fn send_request<S>(
    io: TokioIo<S>,
    host: &str,
    path_and_query: &str,
    method: &Method,
    headers: &HeaderMap,
    body: Bytes,
    request_timeout: Duration,
) -> Result<UpstreamResponse, Box<dyn std::error::Error + Send + Sync>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = http1::handshake(io).await?;

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            debug!("upstream conn closed: {}", e);
        }
    });

    let mut builder = hyper::Request::builder()
        .method(method.as_str())
        .uri(path_and_query)
        .header("host", host);

    for (name, value) in headers.iter() {
        if name.as_str().eq_ignore_ascii_case("host") {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_bytes());
    }

    let req = builder.body(Full::new(body))?;
    let response = tokio::time::timeout(request_timeout, sender.send_request(req))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "upstream timeout"))??;

    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    let mut resp_headers = HeaderMap::new();
    for (name, value) in response.headers() {
        if let (Ok(n), Ok(v)) = (
            axum::http::HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            resp_headers.append(n, v);
        }
    }

    Ok((status, resp_headers, response.into_body()))
}

fn build_redirect_response(
    status: StatusCode,
    resp_headers: &HeaderMap,
    new_location: &str,
) -> Response {
    let filtered = filter_response_headers(resp_headers);
    let mut builder = Response::builder().status(status);
    let hdrs = builder.headers_mut().unwrap();
    for (name, value) in cors_headers() {
        hdrs.insert(name, value);
    }
    for (name, value) in filtered.iter() {
        if name.as_str() != "location" {
            hdrs.append(name.clone(), value.clone());
        }
    }
    if let Ok(loc) = HeaderValue::from_str(new_location) {
        hdrs.insert("location", loc);
    }
    builder.body(Body::empty()).unwrap()
}

fn build_stream_response(
    status: StatusCode,
    resp_headers: &HeaderMap,
    body: hyper::body::Incoming,
) -> Response {
    let filtered = filter_response_headers(resp_headers);
    let mut builder = Response::builder().status(status);
    let hdrs = builder.headers_mut().unwrap();
    for (name, value) in cors_headers() {
        hdrs.insert(name, value);
    }
    for (name, value) in filtered.iter() {
        hdrs.append(name.clone(), value.clone());
    }

    let stream = BodyStream::new(body).filter_map(|frame| async move {
        match frame {
            Ok(f) => f.into_data().ok().map(Ok),
            Err(e) => Some(Err(e)),
        }
    });
    builder.body(Body::from_stream(stream)).unwrap()
}
