use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tracing::{debug, warn};

use super::{parse_proxies, ProxyPool};

/// Poll the proxy file's mtime and hot-reload on change. An external script
/// can rewrite the file to add/remove proxies without restarting the service.
pub async fn run_proxy_watcher(pool: Arc<ProxyPool>, path: PathBuf, interval: Duration) {
    let mut last: Option<SystemTime> = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok());

    loop {
        tokio::time::sleep(interval).await;

        let mtime = match tokio::fs::metadata(&path).await {
            Ok(m) => m.modified().ok(),
            Err(e) => {
                warn!("proxy watcher: cannot stat {:?}: {}", path, e);
                continue;
            }
        };

        if mtime == last {
            continue;
        }
        last = mtime;

        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let parsed = parse_proxies(&content);
                if parsed.is_empty() {
                    warn!("proxy watcher: {:?} now parses to zero proxies — ignoring", path);
                    continue;
                }
                debug!("proxy watcher: change detected, applying {} proxies", parsed.len());
                pool.reload(parsed);
            }
            Err(e) => warn!("proxy watcher: cannot read {:?}: {}", path, e),
        }
    }
}
