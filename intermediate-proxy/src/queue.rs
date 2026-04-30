use std::cmp::Ordering as CmpOrdering;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::Notify;

use crate::upstream::{parse_upstream, Upstream};

pub const DEFAULT_SLOW_THRESHOLD_MS: u64 = 3000;
pub const DEFAULT_UPSTREAM_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_RESORT_INTERVAL_MS: u64 = 2_000;
pub const DEFAULT_HEDGE_DELAY_MS: u64 = 700;
pub const DEFAULT_MAX_PARALLEL_HEDGE: usize = 4;

const FAILED_STREAK: u32 = 2;
const EWMA_NUM: u64 = 3;
const EWMA_DEN: u64 = 10;
const REASON_MAX_LEN: usize = 200;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Tier {
    Healthy = 0,
    Slow = 1,
    Failed = 2,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Healthy => "healthy",
            Tier::Slow => "slow",
            Tier::Failed => "failed",
        }
    }
}

pub struct Entry {
    pub url: String,
    pub is_reserve: bool,
    pub upstream: Upstream,
    pub slow_threshold_ms: u64,

    pub avg_latency_ms: AtomicU64,
    pub last_latency_ms: AtomicU64,
    pub success_count: AtomicU64,
    pub error_count: AtomicU64,
    pub usage_count: AtomicU64,
    pub consecutive_failures: AtomicU32,
    pub consecutive_successes: AtomicU32,
    pub last_success_ms: AtomicU64,
    pub last_error_ms: AtomicU64,
    pub last_error_reason: Mutex<Option<String>>,
}

impl Entry {
    fn new(url: &str, is_reserve: bool, slow_threshold_ms: u64) -> Self {
        Self {
            url: url.to_string(),
            is_reserve,
            upstream: parse_upstream(url),
            slow_threshold_ms,
            avg_latency_ms: AtomicU64::new(0),
            last_latency_ms: AtomicU64::new(0),
            success_count: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            usage_count: AtomicU64::new(0),
            consecutive_failures: AtomicU32::new(0),
            consecutive_successes: AtomicU32::new(0),
            last_success_ms: AtomicU64::new(0),
            last_error_ms: AtomicU64::new(0),
            last_error_reason: Mutex::new(None),
        }
    }

    pub fn tier(&self) -> Tier {
        if self.consecutive_failures.load(Ordering::Relaxed) >= FAILED_STREAK {
            return Tier::Failed;
        }
        let lat = self.avg_latency_ms.load(Ordering::Relaxed);
        if lat > self.slow_threshold_ms {
            return Tier::Slow;
        }
        Tier::Healthy
    }

    fn blend_latency(&self, sample_ms: u64) {
        let prev = self.avg_latency_ms.load(Ordering::Relaxed);
        let next = if prev == 0 {
            sample_ms
        } else {
            (EWMA_NUM * sample_ms + (EWMA_DEN - EWMA_NUM) * prev) / EWMA_DEN
        };
        self.avg_latency_ms.store(next, Ordering::Relaxed);
    }

    fn record_reason(&self, reason: &str) {
        let scrubbed = scrub_credentials(reason);
        let trimmed = if scrubbed.len() > REASON_MAX_LEN {
            format!("{}…", &scrubbed[..REASON_MAX_LEN])
        } else {
            scrubbed
        };
        if let Ok(mut r) = self.last_error_reason.lock() {
            *r = Some(trimmed);
        }
    }

    pub fn observe_success(&self, latency_ms: u64) {
        self.usage_count.fetch_add(1, Ordering::Relaxed);
        self.success_count.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.consecutive_successes.fetch_add(1, Ordering::Relaxed);
        self.last_success_ms.store(now_ms(), Ordering::Relaxed);
        self.last_latency_ms.store(latency_ms, Ordering::Relaxed);
        self.blend_latency(latency_ms);
    }

    /// Upstream answered but with a retryable bad status (429/500/...).
    /// Count as soft failure: bump error stats and penalize the rolling latency
    /// so tiering pushes it out of Healthy.
    pub fn observe_soft_error(&self, latency_ms: u64, reason: &str) {
        self.usage_count.fetch_add(1, Ordering::Relaxed);
        self.error_count.fetch_add(1, Ordering::Relaxed);
        self.consecutive_successes.store(0, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        self.last_error_ms.store(now_ms(), Ordering::Relaxed);
        self.last_latency_ms.store(latency_ms, Ordering::Relaxed);
        let penalty = latency_ms.max(self.slow_threshold_ms + 1);
        self.blend_latency(penalty);
        self.record_reason(reason);
    }

    /// Upstream didn't answer (network error, timeout, stream collapse).
    pub fn observe_hard_error(&self, reason: &str) {
        self.usage_count.fetch_add(1, Ordering::Relaxed);
        self.error_count.fetch_add(1, Ordering::Relaxed);
        self.consecutive_successes.store(0, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        self.last_error_ms.store(now_ms(), Ordering::Relaxed);
        self.record_reason(reason);
    }
}

pub struct ProxyQueue {
    entries: Vec<Arc<Entry>>,
    snapshot: ArcSwap<Vec<Arc<Entry>>>,
    regular_count: usize,
    slow_threshold_ms: u64,
    upstream_timeout: Duration,
    hedge_delay: Duration,
    max_parallel_hedge: usize,
    dirty: AtomicBool,
    all_failed: AtomicBool,
    notify: Notify,
}

impl ProxyQueue {
    pub fn new(
        regular_urls: &[String],
        reserve_urls: &[String],
        slow_threshold_ms: u64,
        upstream_timeout: Duration,
        hedge_delay: Duration,
        max_parallel_hedge: usize,
    ) -> Arc<Self> {
        let mut entries = Vec::with_capacity(regular_urls.len() + reserve_urls.len());
        for url in regular_urls {
            entries.push(Arc::new(Entry::new(url, false, slow_threshold_ms)));
        }
        for url in reserve_urls {
            entries.push(Arc::new(Entry::new(url, true, slow_threshold_ms)));
        }
        let snapshot = ArcSwap::from_pointee(entries.clone());
        Arc::new(Self {
            regular_count: regular_urls.len(),
            entries,
            snapshot,
            slow_threshold_ms,
            upstream_timeout,
            hedge_delay,
            max_parallel_hedge: max_parallel_hedge.max(1),
            dirty: AtomicBool::new(false),
            all_failed: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    #[inline]
    pub fn snapshot(&self) -> Arc<Vec<Arc<Entry>>> {
        self.snapshot.load_full()
    }

    #[inline]
    pub fn upstream_timeout(&self) -> Duration {
        self.upstream_timeout
    }

    #[inline]
    pub fn hedge_delay(&self) -> Duration {
        self.hedge_delay
    }

    #[inline]
    pub fn max_parallel_hedge(&self) -> usize {
        self.max_parallel_hedge
    }

    pub fn record_success(&self, entry: &Entry, latency_ms: u64) {
        let before = entry.tier();
        entry.observe_success(latency_ms);
        self.all_failed.store(false, Ordering::Relaxed);
        if before != entry.tier() {
            self.schedule_resort();
        }
    }

    pub fn record_soft_error(&self, entry: &Entry, latency_ms: u64, reason: &str) {
        let before = entry.tier();
        entry.observe_soft_error(latency_ms, reason);
        if before != entry.tier() {
            self.schedule_resort();
        }
    }

    pub fn record_hard_error(&self, entry: &Entry, reason: &str) {
        let before = entry.tier();
        entry.observe_hard_error(reason);
        if before != entry.tier() {
            self.schedule_resort();
        }
    }

    /// Lookup+record for code paths that only have a URL (e.g. recovery).
    pub fn record_hard_error_url(&self, url: &str, reason: &str) {
        if let Some(entry) = self.entries.iter().find(|e| e.url == url) {
            self.record_hard_error(entry, reason);
        }
    }

    pub fn mark_all_failed(&self) {
        self.all_failed.store(true, Ordering::Relaxed);
    }

    fn schedule_resort(&self) {
        self.dirty.store(true, Ordering::Relaxed);
        self.notify.notify_one();
    }

    pub async fn run_resorter(self: Arc<Self>, period: Duration) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            tokio::select! {
                _ = tokio::time::sleep(period) => {}
                _ = &mut notified => {}
            }
            if self.dirty.swap(false, Ordering::Relaxed) {
                self.resort();
            }
        }
    }

    fn resort(&self) {
        // Freeze sort keys once so the comparator is stable even under
        // concurrent stat updates.
        let mut annotated: Vec<(u8, u8, u64, usize, Arc<Entry>)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let key_reserve = e.is_reserve as u8;
                let key_tier = e.tier() as u8;
                let key_latency = e.avg_latency_ms.load(Ordering::Relaxed);
                (key_reserve, key_tier, key_latency, i, e.clone())
            })
            .collect();

        annotated.sort_by(|a, b| match a.0.cmp(&b.0) {
            CmpOrdering::Equal => match a.1.cmp(&b.1) {
                CmpOrdering::Equal => match a.2.cmp(&b.2) {
                    CmpOrdering::Equal => a.3.cmp(&b.3),
                    other => other,
                },
                other => other,
            },
            other => other,
        });

        let ordered: Vec<Arc<Entry>> =
            annotated.into_iter().map(|(_, _, _, _, e)| e).collect();

        let old = self.snapshot.load();
        let changed = old.len() != ordered.len()
            || old.iter().zip(ordered.iter()).any(|(a, b)| a.url != b.url);

        if changed {
            let rendered: Vec<String> = ordered
                .iter()
                .map(|e| {
                    format!(
                        "{}[{}{} {}ms]",
                        sanitize_url(&e.url),
                        e.tier().as_str(),
                        if e.is_reserve { "/reserve" } else { "" },
                        e.avg_latency_ms.load(Ordering::Relaxed)
                    )
                })
                .collect();
            tracing::info!(
                "Queue reordered (regular={}, reserve={}): {}",
                self.regular_count,
                self.entries.len() - self.regular_count,
                rendered.join(" → ")
            );
            self.snapshot.store(Arc::new(ordered));
        }
    }

    pub fn status(&self) -> serde_json::Value {
        let snap = self.snapshot.load();
        let mut regular = Vec::new();
        let mut reserve = Vec::new();
        let mut tier_counts = [0u64; 3];
        for (i, entry) in snap.iter().enumerate() {
            tier_counts[entry.tier() as usize] += 1;
            let j = entry_json(i, entry);
            if entry.is_reserve {
                reserve.push(j);
            } else {
                regular.push(j);
            }
        }

        serde_json::json!({
            "status": "ok",
            "allProxiesWorking": !self.all_failed.load(Ordering::Relaxed),
            "queueLength": snap.len(),
            "slowThresholdMs": self.slow_threshold_ms,
            "upstreamTimeoutMs": self.upstream_timeout.as_millis() as u64,
            "tiers": {
                "healthy": tier_counts[0],
                "slow": tier_counts[1],
                "failed": tier_counts[2],
            },
            "regular": {
                "total": self.regular_count,
                "proxies": regular,
            },
            "reserve": {
                "total": self.entries.len() - self.regular_count,
                "proxies": reserve,
            }
        })
    }
}

fn entry_json(position: usize, e: &Entry) -> serde_json::Value {
    let reason = e
        .last_error_reason
        .lock()
        .ok()
        .and_then(|g| (*g).clone());
    serde_json::json!({
        "position": position + 1,
        "url": sanitize_url(&e.url),
        "isReserve": e.is_reserve,
        "tier": e.tier().as_str(),
        "avgLatencyMs": e.avg_latency_ms.load(Ordering::Relaxed),
        "lastLatencyMs": e.last_latency_ms.load(Ordering::Relaxed),
        "successCount": e.success_count.load(Ordering::Relaxed),
        "errorCount": e.error_count.load(Ordering::Relaxed),
        "usageCount": e.usage_count.load(Ordering::Relaxed),
        "consecutiveFailures": e.consecutive_failures.load(Ordering::Relaxed),
        "consecutiveSuccesses": e.consecutive_successes.load(Ordering::Relaxed),
        "lastSuccessMs": e.last_success_ms.load(Ordering::Relaxed),
        "lastErrorMs": e.last_error_ms.load(Ordering::Relaxed),
        "lastErrorReason": reason,
    })
}

/// Scrub `user:pass@` credentials from any URL-looking substrings in a
/// free-form text (reqwest error messages frequently embed URLs).
pub fn scrub_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("://") {
        out.push_str(&rest[..idx + 3]);
        let tail = &rest[idx + 3..];
        let boundary = tail
            .find(|c: char| {
                c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '>' | '<' | ',' | ';')
            })
            .unwrap_or(tail.len());
        let chunk = &tail[..boundary];
        let authority_end = chunk.find(['/', '?', '#']).unwrap_or(chunk.len());
        let authority = &chunk[..authority_end];
        let after_authority = &chunk[authority_end..];
        let masked_authority = match authority.rfind('@') {
            Some(at) => {
                let userinfo = &authority[..at];
                let host = &authority[at + 1..];
                let masked_userinfo = match userinfo.find(':') {
                    Some(c) => format!("{}:***", &userinfo[..c]),
                    None => userinfo.to_string(),
                };
                format!("{masked_userinfo}@{host}")
            }
            None => authority.to_string(),
        };
        out.push_str(&masked_authority);
        out.push_str(after_authority);
        rest = &tail[boundary..];
    }
    out.push_str(rest);
    out
}

/// Mask the password portion of `scheme://user:pass@host/...` so we don't
/// leak upstream credentials into logs or the /health endpoint.
pub fn sanitize_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let after_scheme = &url[scheme_end + 3..];
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    let Some(at_pos) = authority.rfind('@') else {
        return url.to_string();
    };
    let userinfo = &authority[..at_pos];
    let host = &authority[at_pos + 1..];
    let masked_userinfo = match userinfo.find(':') {
        Some(colon) => format!("{}:***", &userinfo[..colon]),
        None => userinfo.to_string(),
    };
    format!(
        "{}://{}@{}{}",
        &url[..scheme_end],
        masked_userinfo,
        host,
        &after_scheme[authority_end..]
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
