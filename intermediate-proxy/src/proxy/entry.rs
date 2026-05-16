use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use parking_lot::Mutex;

use crate::upstream::{parse_upstream, Upstream};
use crate::util::now_ms;

pub type RouteId = u32;

const EWMA_NUM: u64 = 3;
const EWMA_DEN: u64 = 10;
const REASON_MAX_LEN: usize = 200;

#[derive(Default)]
pub struct GlobalStats {
    pub success_count: AtomicU64,
    pub error_count: AtomicU64,
    pub usage_count: AtomicU64,
    pub last_success_ms: AtomicU64,
    pub last_error_ms: AtomicU64,
}

/// Per-route, per-proxy health. Created lazily the first time a route touches
/// a proxy.
pub struct RouteState {
    pub banned_until_ms: AtomicU64,
    pub consecutive_failures: AtomicU32,
    pub consecutive_successes: AtomicU32,
    pub avg_latency_ms: AtomicU64,
    pub last_latency_ms: AtomicU64,
    pub success_count: AtomicU64,
    pub error_count: AtomicU64,
    pub last_success_ms: AtomicU64,
    pub last_error_ms: AtomicU64,
    pub last_probe_ms: AtomicU64,
    pub ban_count: AtomicU64,
    pub last_error_reason: Mutex<Option<String>>,
}

impl Default for RouteState {
    fn default() -> Self {
        Self {
            banned_until_ms: AtomicU64::new(0),
            consecutive_failures: AtomicU32::new(0),
            consecutive_successes: AtomicU32::new(0),
            avg_latency_ms: AtomicU64::new(0),
            last_latency_ms: AtomicU64::new(0),
            success_count: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            last_success_ms: AtomicU64::new(0),
            last_error_ms: AtomicU64::new(0),
            last_probe_ms: AtomicU64::new(0),
            ban_count: AtomicU64::new(0),
            last_error_reason: Mutex::new(None),
        }
    }
}

impl RouteState {
    fn blend_latency(&self, sample_ms: u64) {
        let prev = self.avg_latency_ms.load(Ordering::Relaxed);
        let next = if prev == 0 {
            sample_ms
        } else {
            (EWMA_NUM * sample_ms + (EWMA_DEN - EWMA_NUM) * prev) / EWMA_DEN
        };
        self.avg_latency_ms.store(next, Ordering::Relaxed);
    }

    fn set_reason(&self, reason: &str) {
        let scrubbed = crate::util::scrub_credentials(reason);
        let trimmed = if scrubbed.len() > REASON_MAX_LEN {
            let mut end = REASON_MAX_LEN;
            while !scrubbed.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &scrubbed[..end])
        } else {
            scrubbed
        };
        *self.last_error_reason.lock() = Some(trimmed);
    }

    pub fn is_banned(&self, now: u64) -> bool {
        self.banned_until_ms.load(Ordering::Relaxed) > now
    }

    pub fn ban_for(&self, ms: u64) {
        let until = now_ms().saturating_add(ms);
        self.banned_until_ms.store(until, Ordering::Relaxed);
        self.ban_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn ban_permanent(&self) {
        self.banned_until_ms.store(u64::MAX, Ordering::Relaxed);
        self.ban_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn clear_ban(&self) {
        self.banned_until_ms.store(0, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    pub fn observe_success(&self, latency_ms: u64) {
        self.success_count.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.consecutive_successes.fetch_add(1, Ordering::Relaxed);
        self.last_success_ms.store(now_ms(), Ordering::Relaxed);
        self.last_latency_ms.store(latency_ms, Ordering::Relaxed);
        self.blend_latency(latency_ms);
    }

    pub fn observe_failure(&self, latency_ms: Option<u64>, reason: &str, slow_ms: u64) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
        self.consecutive_successes.store(0, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        self.last_error_ms.store(now_ms(), Ordering::Relaxed);
        if let Some(l) = latency_ms {
            self.last_latency_ms.store(l, Ordering::Relaxed);
            self.blend_latency(l.max(slow_ms + 1));
        }
        self.set_reason(reason);
    }
}

pub struct Entry {
    pub url: String,
    pub upstream: Upstream,
    tags: ArcSwap<Vec<String>>,
    pub global: GlobalStats,
    pub transport_fatal: AtomicBool,
    pub transport_fatal_since_ms: AtomicU64,
    pub transport_fatal_count: AtomicU64,
    pub round_robin: AtomicUsize,
    by_route: DashMap<RouteId, Arc<RouteState>, ahash::RandomState>,
}

impl Entry {
    pub fn new(url: &str, tags: Vec<String>) -> Self {
        Self {
            url: url.to_string(),
            upstream: parse_upstream(url),
            tags: ArcSwap::from_pointee(tags),
            global: GlobalStats::default(),
            transport_fatal: AtomicBool::new(false),
            transport_fatal_since_ms: AtomicU64::new(0),
            transport_fatal_count: AtomicU64::new(0),
            round_robin: AtomicUsize::new(0),
            by_route: DashMap::with_hasher(ahash::RandomState::new()),
        }
    }

    pub fn tags(&self) -> arc_swap::Guard<Arc<Vec<String>>> {
        self.tags.load()
    }

    pub fn set_tags(&self, tags: Vec<String>) {
        self.tags.store(Arc::new(tags));
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.load().iter().any(|t| t == tag)
    }

    pub fn is_reserve(&self) -> bool {
        self.has_tag("reserve")
    }

    pub fn route_state(&self, rid: RouteId) -> Arc<RouteState> {
        if let Some(s) = self.by_route.get(&rid) {
            return s.clone();
        }
        self.by_route
            .entry(rid)
            .or_insert_with(|| Arc::new(RouteState::default()))
            .clone()
    }

    pub fn route_states(&self) -> Vec<(RouteId, Arc<RouteState>)> {
        self.by_route
            .iter()
            .map(|kv| (*kv.key(), kv.value().clone()))
            .collect()
    }

    pub fn mark_transport_fatal(&self) -> bool {
        let was = self.transport_fatal.swap(true, Ordering::Relaxed);
        if !was {
            self.transport_fatal_since_ms
                .store(now_ms(), Ordering::Relaxed);
            self.transport_fatal_count.fetch_add(1, Ordering::Relaxed);
        }
        !was
    }

    pub fn clear_transport_fatal(&self) -> bool {
        let was = self.transport_fatal.swap(false, Ordering::Relaxed);
        if was {
            self.transport_fatal_since_ms.store(0, Ordering::Relaxed);
        }
        was
    }

    pub fn is_transport_fatal(&self) -> bool {
        self.transport_fatal.load(Ordering::Relaxed)
    }
}

/// A connect-class transport error means the proxy is unreachable for *every*
/// route, so it gets the global fatal flag rather than a per-route ban.
///
/// A request *timeout* is deliberately NOT here: under load a slow target
/// makes healthy proxies time out, and globally sidelining them collapses the
/// pool. Timeouts stay a per-route soft failure instead.
pub fn is_transport_fatal_reason(reason: &str) -> bool {
    reason.contains("client error (Connect)")
        || reason.contains("dns error")
        || reason.contains("Connection refused")
}
