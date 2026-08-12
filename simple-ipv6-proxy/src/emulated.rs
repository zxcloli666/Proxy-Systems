use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use futures_util::StreamExt;
use std::net::{IpAddr, SocketAddr, SocketAddrV6};
use std::time::Duration;

#[derive(Clone)]
pub struct Emulator {
    emulation: wreq_util::Emulation,
}

impl Emulator {
    pub fn new(profile: &str) -> Result<Self, String> {
        let emulation: wreq_util::Emulation =
            serde_json::from_value(serde_json::Value::String(profile.to_string()))
                .map_err(|_| format!("unknown impersonation profile: {profile}"))?;
        Ok(Self { emulation })
    }

    pub async fn send(
        &self,
        target: SocketAddrV6,
        source: Option<IpAddr>,
        url: &str,
        host: &str,
        method: &Method,
        headers: &HeaderMap,
        body: Bytes,
        request_timeout: Duration,
    ) -> Result<(StatusCode, HeaderMap, Body), Box<dyn std::error::Error + Send + Sync>> {
        let mut builder = wreq::Client::builder()
            .emulation(self.emulation)
            .redirect(wreq::redirect::Policy::none())
            .timeout(request_timeout)
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
