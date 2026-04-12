use std::sync::Arc;
use tokio::sync::RwLock;

use crate::upstream::{parse_upstream, Upstream};

/// Snapshot of the proxy queue at a point in time.
pub struct QueueSnapshot {
    pub proxies: Vec<Upstream>,
    pub version: u64,
}

/// Stats for a single proxy entry.
#[derive(Clone)]
pub struct ProxyStats {
    pub url: String,
    pub is_reserve: bool,
    pub last_success: Option<String>,
    pub last_error: Option<String>,
    pub usage_count: u64,
}

/// Thread-safe proxy priority queue with versioned reordering.
pub struct ProxyQueue {
    inner: RwLock<QueueInner>,
}

struct QueueInner {
    proxies: Vec<Upstream>,
    stats: Vec<ProxyStats>,
    version: u64,
    regular_count: usize,
    all_working: bool,
    last_checked: Option<String>,
}

impl ProxyQueue {
    /// Create a new proxy queue from regular and reserve proxy URL lists.
    pub fn new(regular_urls: &[String], reserve_urls: &[String]) -> Arc<Self> {
        let mut proxies = Vec::new();
        let mut stats = Vec::new();

        for url in regular_urls {
            proxies.push(parse_upstream(url));
            stats.push(ProxyStats {
                url: url.clone(),
                is_reserve: false,
                last_success: None,
                last_error: None,
                usage_count: 0,
            });
        }

        for url in reserve_urls {
            proxies.push(parse_upstream(url));
            stats.push(ProxyStats {
                url: url.clone(),
                is_reserve: true,
                last_success: None,
                last_error: None,
                usage_count: 0,
            });
        }

        Arc::new(Self {
            inner: RwLock::new(QueueInner {
                regular_count: regular_urls.len(),
                proxies,
                stats,
                version: 0,
                all_working: true,
                last_checked: None,
            }),
        })
    }

    /// Get a snapshot of the current queue order and version.
    pub async fn snapshot(&self) -> QueueSnapshot {
        let inner = self.inner.read().await;
        QueueSnapshot {
            proxies: inner.proxies.clone(),
            version: inner.version,
        }
    }

    /// Move a failed proxy to the end of its zone (regular or reserve).
    /// Only modifies if the queue version matches the snapshot version.
    pub async fn move_to_end(&self, proxy_url: &str, snapshot_version: u64) {
        let mut inner = self.inner.write().await;

        if inner.version != snapshot_version {
            tracing::warn!(
                "Queue version changed ({} -> {}), skipping reorder for {}",
                snapshot_version,
                inner.version,
                proxy_url
            );
            return;
        }

        let Some(index) = inner.proxies.iter().position(|p| p.url == proxy_url) else {
            return;
        };

        let is_reserve = inner.stats[index].is_reserve;

        if is_reserve {
            // Move to end of reserve zone (absolute end)
            let last = inner.proxies.len() - 1;
            if index != last {
                let proxy = inner.proxies.remove(index);
                let stat = inner.stats.remove(index);
                inner.proxies.push(proxy);
                inner.stats.push(stat);
                inner.version += 1;
                tracing::info!(
                    "Moved RESERVE {} to end of reserve zone (version {})",
                    proxy_url,
                    inner.version
                );
            }
        } else {
            // Move to end before reserve zone
            let first_reserve = inner.stats.iter().position(|s| s.is_reserve);
            let target = first_reserve.unwrap_or(inner.proxies.len());

            if index < target - 1 {
                let proxy = inner.proxies.remove(index);
                let stat = inner.stats.remove(index);
                inner.proxies.insert(target - 1, proxy);
                inner.stats.insert(target - 1, stat);
                inner.version += 1;
                tracing::info!(
                    "Moved {} to end of regular zone (version {})",
                    proxy_url,
                    inner.version
                );
            }
        }
    }

    /// Mark a proxy as successful.
    pub async fn mark_success(&self, proxy_url: &str) {
        let mut inner = self.inner.write().await;
        if let Some(stat) = inner.stats.iter_mut().find(|s| s.url == proxy_url) {
            stat.last_success = Some(now());
            stat.usage_count += 1;
        }
        inner.all_working = true;
        inner.last_checked = Some(now());
    }

    /// Mark a proxy as errored.
    pub async fn mark_error(&self, proxy_url: &str) {
        let mut inner = self.inner.write().await;
        if let Some(stat) = inner.stats.iter_mut().find(|s| s.url == proxy_url) {
            stat.last_error = Some(now());
        }
        inner.last_checked = Some(now());
    }

    /// Mark all proxies as failed.
    pub async fn mark_all_failed(&self) {
        let mut inner = self.inner.write().await;
        inner.all_working = false;
        inner.last_checked = Some(now());
    }

    /// Get queue status for the health endpoint.
    pub async fn status(&self) -> serde_json::Value {
        let inner = self.inner.read().await;

        let regular: Vec<_> = inner
            .stats
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.is_reserve)
            .map(|(i, s)| stat_json(i, s))
            .collect();

        let reserve: Vec<_> = inner
            .stats
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_reserve)
            .map(|(i, s)| stat_json(i, s))
            .collect();

        serde_json::json!({
            "status": "ok",
            "allProxiesWorking": inner.all_working,
            "lastChecked": inner.last_checked,
            "queueVersion": inner.version,
            "regular": {
                "total": inner.regular_count,
                "proxies": regular,
            },
            "reserve": {
                "total": inner.stats.iter().filter(|s| s.is_reserve).count(),
                "proxies": reserve,
            }
        })
    }
}

fn stat_json(position: usize, s: &ProxyStats) -> serde_json::Value {
    serde_json::json!({
        "position": position + 1,
        "url": s.url,
        "isReserve": s.is_reserve,
        "lastSuccess": s.last_success,
        "lastError": s.last_error,
        "usageCount": s.usage_count,
    })
}

fn now() -> String {
    // Simple ISO-ish timestamp without external crate
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}s", dur.as_secs())
}
