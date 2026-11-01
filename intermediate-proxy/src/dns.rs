use dashmap::DashMap;
use hickory_resolver::TokioResolver;
use std::net::IpAddr;
use std::time::{Duration, Instant};
use tracing::warn;

const FOUND_TTL: Duration = Duration::from_secs(300);
const MISSING_TTL: Duration = Duration::from_secs(60);
const MAX_HOSTS: usize = 10_000;

pub struct DnsCheck {
    resolver: TokioResolver,
    known: DashMap<String, (bool, Instant)>,
}

impl DnsCheck {
    pub fn from_system() -> Option<Self> {
        let resolver = TokioResolver::builder_tokio().and_then(|b| b.build());
        match resolver {
            Ok(resolver) => Some(Self {
                resolver,
                known: DashMap::new(),
            }),
            Err(e) => {
                warn!("dns check off, no system resolver: {e}");
                None
            }
        }
    }

    pub async fn missing(&self, host: &str) -> bool {
        if host.is_empty() || host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok() {
            return false;
        }
        let now = Instant::now();
        if let Some(entry) = self.known.get(host) {
            let (found, until) = *entry;
            if until > now {
                return !found;
            }
        }
        let found = match self.resolver.lookup_ip(host).await {
            Ok(_) => true,
            Err(e) if e.is_nx_domain() || e.is_no_records_found() => false,
            Err(_) => return false,
        };
        if self.known.len() >= MAX_HOSTS {
            self.known.clear();
        }
        let ttl = if found { FOUND_TTL } else { MISSING_TTL };
        self.known.insert(host.to_string(), (found, now + ttl));
        !found
    }
}
