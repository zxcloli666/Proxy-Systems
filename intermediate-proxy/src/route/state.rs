use std::hash::{Hash, Hasher};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::proxy::{Entry, RouteState};
use crate::route::config::{RouteDef, Selector};
use crate::util::now_ms;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Tier {
    Healthy = 0,
    Slow = 1,
    Failed = 2,
    Banned = 3,
    Fatal = 4,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Healthy => "healthy",
            Tier::Slow => "slow",
            Tier::Failed => "failed",
            Tier::Banned => "banned",
            Tier::Fatal => "fatal",
        }
    }
}

pub fn entry_tier(entry: &Entry, st: &RouteState, route: &RouteDef, now: u64) -> Tier {
    if entry.is_transport_fatal() {
        return Tier::Fatal;
    }
    if st.is_banned(now) {
        return Tier::Banned;
    }
    if st.consecutive_failures.load(Ordering::Relaxed) >= route.fail_streak {
        return Tier::Failed;
    }
    let lat = st.avg_latency_ms.load(Ordering::Relaxed);
    if lat > 0 && lat > route.slow_ms {
        return Tier::Slow;
    }
    Tier::Healthy
}

fn tag_match(entry: &Entry, all: &[String], any: &[String]) -> bool {
    if !all.is_empty() {
        let tags = entry.tags();
        for need in all {
            if !tags.iter().any(|t| t == need) {
                return false;
            }
        }
    }
    if !any.is_empty() {
        let tags = entry.tags();
        if !any.iter().any(|a| tags.iter().any(|t| t == a)) {
            return false;
        }
    }
    true
}

pub struct Candidate {
    pub entry: Arc<Entry>,
    pub tier: Tier,
    pub latency: u64,
}

/// Build the ordered attempt plan for a route: tag-filtered, live (non-banned,
/// non-fatal) proxies sorted by the route's selector, with non-reserve before
/// reserve when `prefer_non_reserve`. Capped to `max_attempts`, but slow/failed
/// proxies backfill if not enough healthy ones exist.
pub fn build_plan(
    route: &RouteDef,
    snapshot: &[Arc<Entry>],
    pool_all: &[String],
    pool_any: &[String],
    sticky_key: Option<&str>,
) -> Vec<Candidate> {
    let now = now_ms();
    let mut live: Vec<Candidate> = Vec::with_capacity(snapshot.len().min(64));

    for e in snapshot.iter() {
        if !tag_match(e, pool_all, pool_any) {
            continue;
        }
        let st = e.route_state(route.id);
        let tier = entry_tier(e, &st, route, now);
        if tier == Tier::Banned || tier == Tier::Fatal {
            continue;
        }
        let latency = st.avg_latency_ms.load(Ordering::Relaxed);
        live.push(Candidate {
            entry: Arc::clone(e),
            tier,
            latency,
        });
    }

    order(route, &mut live, sticky_key);

    if route.prefer_non_reserve {
        live.sort_by_key(|c| c.entry.is_reserve());
    }

    live.truncate(route.max_attempts);
    live
}

/// Proxies excluded from the live plan (banned or transport-fatal) that the
/// `try_fatal` exhausted path may still attempt. Not tag-filtered beyond the
/// route pool; ordering is best-effort by latency.
pub fn fallback_pool(
    route: &RouteDef,
    snapshot: &[Arc<Entry>],
    pool_all: &[String],
    pool_any: &[String],
    tried: &std::collections::HashSet<String>,
) -> Vec<Arc<Entry>> {
    let now = now_ms();
    let mut out: Vec<(u64, Arc<Entry>)> = Vec::new();
    for e in snapshot.iter() {
        if tried.contains(&e.url) {
            continue;
        }
        if !tag_match(e, pool_all, pool_any) {
            continue;
        }
        let st = e.route_state(route.id);
        let tier = entry_tier(e, &st, route, now);
        if tier == Tier::Banned || tier == Tier::Fatal {
            out.push((st.avg_latency_ms.load(Ordering::Relaxed), Arc::clone(e)));
        }
    }
    out.sort_by_key(|(l, _)| if *l == 0 { u64::MAX } else { *l });
    out.into_iter().map(|(_, e)| e).collect()
}

fn order(route: &RouteDef, live: &mut [Candidate], sticky_key: Option<&str>) {
    match &route.selector {
        Selector::BestLatency | Selector::Hedge => {
            live.sort_by(|a, b| {
                a.tier
                    .cmp(&b.tier)
                    .then_with(|| lat_key(a.latency).cmp(&lat_key(b.latency)))
            });
        }
        Selector::Random => {
            live.sort_by_key(|a| a.tier);
            // shuffle within the whole slice; tier-stable enough for dispatch.
            let n = live.len();
            for i in (1..n).rev() {
                let j = fastrand::usize(..=i);
                live.swap(i, j);
            }
            live.sort_by_key(|a| a.tier);
        }
        Selector::RoundRobin => {
            live.sort_by(|a, b| {
                a.tier
                    .cmp(&b.tier)
                    .then_with(|| lat_key(a.latency).cmp(&lat_key(b.latency)))
            });
            let n = live.len();
            if n > 1 {
                let start = route.cursor.fetch_add(1, Ordering::Relaxed) % n;
                live.rotate_left(start);
            }
        }
        Selector::StickyByHeader(_) => {
            live.sort_by(|a, b| {
                a.tier
                    .cmp(&b.tier)
                    .then_with(|| lat_key(a.latency).cmp(&lat_key(b.latency)))
            });
            let n = live.len();
            if n > 1 {
                let key = sticky_key.unwrap_or("");
                let mut hasher = ahash::AHasher::default();
                key.hash(&mut hasher);
                let h = hasher.finish() as usize;
                live.rotate_left(h % n);
            }
        }
    }
}

#[inline]
fn lat_key(l: u64) -> u64 {
    if l == 0 {
        // unmeasured proxies sort right after the fastest known, not last,
        // so fresh proxies get exercised.
        1
    } else {
        l
    }
}
