pub mod endpoint;
pub mod http_proxy;
pub mod socks;

use reqwest::Client;
use std::time::Duration;

/// Kind of upstream proxy, determined by URL scheme.
#[derive(Debug, Clone)]
pub enum UpstreamKind {
    /// Forward via X-Target header to a proxy endpoint (http/https scheme).
    Endpoint { url: String, client: Client },
    /// Forward via an HTTP forward proxy (forward:// scheme).
    HttpProxy { client: Client },
    /// Forward via a SOCKS5 proxy (socks5:// scheme).
    Socks5 { client: Client },
}

/// An upstream proxy entry carrying the configured client(s) for its scheme.
#[derive(Debug, Clone)]
pub struct Upstream {
    pub kind: UpstreamKind,
}

fn base_client_builder() -> reqwest::ClientBuilder {
    // http1_only is mandatory: workers.dev hosts don't speak HTTP/2 properly
    // through our upstreams in all paths.
    Client::builder()
        .http1_only()
        .connect_timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(256)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_nodelay(true)
        .redirect(reqwest::redirect::Policy::none())
}

/// Parse a proxy URL into an Upstream.
///
/// Schemes:
/// - `http://` or `https://` → Endpoint (forward with X-Target header)
/// - `socks5://host:port` → SOCKS5 proxy
/// - `forward://host:port` → HTTP forward proxy (converted to http://)
pub fn parse_upstream(url: &str) -> Upstream {
    let url_trimmed = url.trim();

    if url_trimmed.starts_with("socks5://") {
        let client = base_client_builder()
            .proxy(reqwest::Proxy::all(url_trimmed).expect("invalid socks5 proxy URL"))
            .build()
            .expect("failed to build socks5 client");
        Upstream {
            kind: UpstreamKind::Socks5 { client },
        }
    } else if url_trimmed.starts_with("forward://") {
        let http_url = url_trimmed.replacen("forward://", "http://", 1);
        let client = base_client_builder()
            .proxy(reqwest::Proxy::all(&http_url).expect("invalid forward proxy URL"))
            .build()
            .expect("failed to build http proxy client");
        Upstream {
            kind: UpstreamKind::HttpProxy { client },
        }
    } else {
        // http:// or https:// → Endpoint
        let client = base_client_builder()
            .no_proxy()
            .build()
            .expect("failed to build endpoint client");
        Upstream {
            kind: UpstreamKind::Endpoint {
                url: url_trimmed.to_string(),
                client,
            },
        }
    }
}
