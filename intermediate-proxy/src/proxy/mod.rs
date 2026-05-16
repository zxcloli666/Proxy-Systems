pub mod entry;
pub mod loader;
pub mod watcher;

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tracing::{info, warn};

pub use entry::{is_transport_fatal_reason, Entry, RouteId, RouteState};
pub use loader::{parse_proxies, ParsedProxy};

use crate::util::sanitize_url;

/// Live, hot-reloadable set of upstream proxies.
///
/// Reads never lock: the ordered list lives in an `ArcSwap`. Reloads diff by
/// URL so existing proxies keep their accumulated per-route stats; only added
/// proxies start cold and removed ones are dropped.
pub struct ProxyPool {
    entries: ArcSwap<Vec<Arc<Entry>>>,
    by_url: ArcSwap<HashMap<String, Arc<Entry>>>,
}

impl ProxyPool {
    pub fn new(parsed: Vec<ParsedProxy>) -> Arc<Self> {
        let mut entries = Vec::with_capacity(parsed.len());
        let mut by_url = HashMap::with_capacity(parsed.len());
        for p in parsed {
            let e = Arc::new(Entry::new(&p.url, p.tags));
            by_url.insert(e.url.clone(), Arc::clone(&e));
            entries.push(e);
        }
        Arc::new(Self {
            entries: ArcSwap::from_pointee(entries),
            by_url: ArcSwap::from_pointee(by_url),
        })
    }

    #[inline]
    pub fn snapshot(&self) -> Arc<Vec<Arc<Entry>>> {
        self.entries.load_full()
    }

    pub fn find(&self, url: &str) -> Option<Arc<Entry>> {
        self.by_url.load().get(url).cloned()
    }

    pub fn len(&self) -> usize {
        self.entries.load().len()
    }

    /// Diff-apply a freshly parsed proxy list. Existing entries keep state.
    pub fn reload(&self, parsed: Vec<ParsedProxy>) {
        let old = self.by_url.load_full();
        let mut new_entries: Vec<Arc<Entry>> = Vec::with_capacity(parsed.len());
        let mut new_by_url: HashMap<String, Arc<Entry>> = HashMap::with_capacity(parsed.len());
        let mut added = 0usize;
        let mut retained = 0usize;

        for p in &parsed {
            if let Some(existing) = old.get(&p.url) {
                if existing.tags().as_slice() != p.tags.as_slice() {
                    existing.set_tags(p.tags.clone());
                }
                new_by_url.insert(p.url.clone(), Arc::clone(existing));
                new_entries.push(Arc::clone(existing));
                retained += 1;
            } else {
                let e = Arc::new(Entry::new(&p.url, p.tags.clone()));
                new_by_url.insert(p.url.clone(), Arc::clone(&e));
                new_entries.push(e);
                added += 1;
            }
        }

        let removed = old.len().saturating_sub(retained);
        if added == 0 && removed == 0 && retained == old.len() {
            return;
        }

        self.entries.store(Arc::new(new_entries));
        self.by_url.store(Arc::new(new_by_url));
        info!(
            "proxy reload: {} total ({} added, {} removed, {} retained)",
            parsed.len(),
            added,
            removed,
            retained
        );
    }

    pub fn summary(&self) -> serde_json::Value {
        let snap = self.entries.load();
        let mut fatal = 0u64;
        for e in snap.iter() {
            if e.is_transport_fatal() {
                fatal += 1;
            }
        }
        serde_json::json!({
            "total": snap.len(),
            "transportFatal": fatal,
        })
    }
}

/// Initial blocking load at startup. Falls back to an empty pool with a loud
/// warning so the service still answers `/health` instead of crash-looping.
pub fn load_initial(path: &str) -> Vec<ParsedProxy> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let parsed = parse_proxies(&content);
            if parsed.is_empty() {
                warn!("proxy file {} parsed to zero proxies", path);
            } else {
                info!(
                    "loaded {} proxies from {}: {}",
                    parsed.len(),
                    path,
                    parsed
                        .iter()
                        .take(8)
                        .map(|p| sanitize_url(&p.url))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            parsed
        }
        Err(e) => {
            warn!("cannot read proxy file {}: {} — starting empty", path, e);
            Vec::new()
        }
    }
}
