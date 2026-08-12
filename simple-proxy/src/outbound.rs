use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;

pub enum Outbound {
    Plain(reqwest::Client),
    Emulated(Box<wreq::Client>),
}

pub struct UpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub stream: BoxStream<'static, Result<Bytes, std::io::Error>>,
}

fn copy_headers<'a, I>(pairs: I) -> HeaderMap
where
    I: IntoIterator<Item = (&'a [u8], &'a [u8])>,
{
    let mut out = HeaderMap::new();
    for (name, value) in pairs {
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name), HeaderValue::from_bytes(value)) {
            out.append(n, v);
        }
    }
    out
}

impl Outbound {
    pub fn plain(client: reqwest::Client) -> Self {
        Outbound::Plain(client)
    }

    pub fn emulated(profile: &str) -> Result<Self, String> {
        let emulation: wreq_util::Emulation =
            serde_json::from_value(serde_json::Value::String(profile.to_string()))
                .map_err(|_| format!("неизвестный профиль имперсонации: {profile}"))?;
        let client = wreq::Client::builder()
            .emulation(emulation)
            .redirect(wreq::redirect::Policy::none())
            .build()
            .map_err(|e| format!("не смог собрать клиент имперсонации: {e}"))?;
        Ok(Outbound::Emulated(Box::new(client)))
    }

    pub async fn send(
        &self,
        method: &Method,
        url: &str,
        headers: &HeaderMap,
        body: Option<Bytes>,
    ) -> Result<UpstreamResponse, String> {
        match self {
            Outbound::Plain(client) => {
                let mut builder = client.request(method.clone(), url);
                let mut out = reqwest::header::HeaderMap::new();
                for (name, value) in headers.iter() {
                    if let (Ok(n), Ok(v)) = (
                        reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
                        reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
                    ) {
                        out.append(n, v);
                    }
                }
                builder = builder.headers(out);
                if let Some(body) = body {
                    builder = builder.body(body);
                }
                let response = builder.send().await.map_err(|e| e.to_string())?;
                let status = StatusCode::from_u16(response.status().as_u16())
                    .unwrap_or(StatusCode::BAD_GATEWAY);
                let headers = copy_headers(
                    response
                        .headers()
                        .iter()
                        .map(|(n, v)| (n.as_str().as_bytes(), v.as_bytes()))
                        .collect::<Vec<_>>(),
                );
                let stream = response
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(std::io::Error::other))
                    .boxed();
                Ok(UpstreamResponse {
                    status,
                    headers,
                    stream,
                })
            }
            Outbound::Emulated(client) => {
                let wreq_method =
                    wreq::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(wreq::Method::GET);
                let mut builder = client.request(wreq_method, url);
                for (name, value) in headers.iter() {
                    builder = builder.header(name.as_str(), value.as_bytes());
                }
                if let Some(body) = body {
                    builder = builder.body(body);
                }
                let response = builder.send().await.map_err(|e| e.to_string())?;
                let status = StatusCode::from_u16(response.status().as_u16())
                    .unwrap_or(StatusCode::BAD_GATEWAY);
                let headers = copy_headers(
                    response
                        .headers()
                        .iter()
                        .map(|(n, v)| (n.as_str().as_bytes(), v.as_bytes()))
                        .collect::<Vec<_>>(),
                );
                let stream = response
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(std::io::Error::other))
                    .boxed();
                Ok(UpstreamResponse {
                    status,
                    headers,
                    stream,
                })
            }
        }
    }
}
