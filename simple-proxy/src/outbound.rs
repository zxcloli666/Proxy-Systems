use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::time::Duration;

pub const DEFAULT_PROFILE: &str = "chrome_137";

pub struct Outbound {
    client: wreq::Client,
    profile: String,
}

pub struct UpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub stream: BoxStream<'static, Result<Bytes, std::io::Error>>,
}

fn parse_profile(name: &str) -> Option<wreq_util::Emulation> {
    serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
}

impl Outbound {
    pub fn new(requested: Option<&str>) -> Result<Self, String> {
        let requested = requested
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

        let client = wreq::Client::builder()
            .emulation(emulation)
            .redirect(wreq::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("failed to build client ({profile}): {e}"))?;

        Ok(Self { client, profile })
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub async fn send(
        &self,
        method: &Method,
        url: &str,
        headers: &HeaderMap,
        body: Option<Bytes>,
    ) -> Result<UpstreamResponse, String> {
        let wreq_method =
            wreq::Method::from_bytes(method.as_str().as_bytes()).map_err(|e| e.to_string())?;

        let mut builder = self.client.request(wreq_method, url);
        for (name, value) in headers.iter() {
            builder = builder.header(name.as_str(), value.as_bytes());
        }
        if let Some(body) = body {
            builder = builder.body(body);
        }

        let response = builder.send().await.map_err(|e| e.to_string())?;
        let status =
            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

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
            .map(|chunk| chunk.map_err(std::io::Error::other))
            .boxed();

        Ok(UpstreamResponse {
            status,
            headers: out_headers,
            stream,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_is_known() {
        assert!(parse_profile(DEFAULT_PROFILE).is_some());
    }

    #[test]
    fn unset_profile_uses_the_default() {
        let outbound = Outbound::new(None).expect("outbound");
        assert_eq!(outbound.profile(), DEFAULT_PROFILE);
    }

    #[test]
    fn unknown_profile_falls_back_instead_of_failing() {
        let outbound = Outbound::new(Some("netscape_1")).expect("outbound");
        assert_eq!(outbound.profile(), DEFAULT_PROFILE);
    }

    #[test]
    fn explicit_profile_is_honoured() {
        let outbound = Outbound::new(Some("firefox_139")).expect("outbound");
        assert_eq!(outbound.profile(), "firefox_139");
    }
}
