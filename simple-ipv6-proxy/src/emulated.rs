use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use futures_util::StreamExt;
use std::net::{IpAddr, SocketAddr, SocketAddrV6};
use std::time::Duration;

pub const DEFAULT_PROFILE: &str = "chrome_137";

pub struct Hop<'a> {
    pub target: SocketAddrV6,
    pub source: Option<IpAddr>,
    pub url: &'a str,
    pub host: &'a str,
    pub method: &'a Method,
    pub headers: &'a HeaderMap,
    pub body: Bytes,
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
}

#[derive(Clone)]
pub struct Emulator {
    emulation: wreq_util::Emulation,
    profile: String,
}

fn parse_profile(name: &str) -> Option<wreq_util::Emulation> {
    serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
}

impl Emulator {
    pub fn new(profile: Option<&str>) -> Result<Self, String> {
        let requested = profile
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .unwrap_or(DEFAULT_PROFILE);

        let (profile, emulation) = match parse_profile(requested) {
            Some(e) => (requested.to_string(), e),
            None => {
                tracing::warn!(
                    requested,
                    fallback = DEFAULT_PROFILE,
                    "unknown impersonation profile"
                );
                let e = parse_profile(DEFAULT_PROFILE)
                    .ok_or_else(|| format!("built-in profile {DEFAULT_PROFILE} is unknown"))?;
                (DEFAULT_PROFILE.to_string(), e)
            }
        };

        Ok(Self { emulation, profile })
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub async fn send(
        &self,
        hop: Hop<'_>,
    ) -> Result<(StatusCode, HeaderMap, Body), Box<dyn std::error::Error + Send + Sync>> {
        let Hop {
            target,
            source,
            url,
            host,
            method,
            headers,
            body,
            request_timeout,
            connect_timeout,
        } = hop;

        let mut builder = wreq::Client::builder()
            .emulation(self.emulation)
            .redirect(wreq::redirect::Policy::none())
            .timeout(request_timeout)
            .connect_timeout(connect_timeout)
            .resolve_to_addrs(host, &[SocketAddr::V6(target)]);

        if let Some(src) = source {
            builder = builder.local_address(src);
        }

        let client = builder.build()?;

        let wreq_method = wreq::Method::from_bytes(method.as_str().as_bytes())?;
        let mut request = client.request(wreq_method, url);
        for (name, value) in headers.iter() {
            request = request.header(name.as_str(), value.as_bytes());
        }
        if !body.is_empty() {
            request = request.body(body);
        }

        let response = request.send().await?;
        let status = StatusCode::from_u16(response.status().as_u16())?;

        let mut out_headers = HeaderMap::new();
        for (name, value) in response.headers().iter() {
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_str().as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                out_headers.append(n, v);
            }
        }

        let stream = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other));

        Ok((status, out_headers, Body::from_stream(stream)))
    }
}
