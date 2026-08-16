use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::forward::probe_request;
use crate::lua::{LuaPool, ProbeOutcome};
use crate::proxy::ProxyPool;
use crate::route::state::{entry_tier, Tier};
use crate::util::{now_ms, sanitize_url};

/// Per-route prober. Every tick it walks each route's probe config and, for
/// proxies banned on that route whose probe interval has elapsed, issues the
/// route's probe request and clears/extends the ban based on the verdict.
/// Concurrency is bounded by a shared semaphore so a large fatal set can't
/// stampede the network.
pub async fn run_route_prober(
    pool: Arc<ProxyPool>,
    lua: Arc<LuaPool>,
    max_concurrent: usize,
) {
    let sem = Arc::new(Semaphore::new(max_concurrent.max(1)));
    let tick = Duration::from_secs(1);

    loop {
        tokio::time::sleep(tick).await;
        let cfg = lua.config();
        let snap = pool.snapshot();
        let now = now_ms();

        for route in &cfg.routes {
            let Some(probe) = &route.probe else {
                continue;
            };
            for entry in snap.iter() {
                let st = entry.route_state(route.id);
                let tier = entry_tier(entry, &st, route, now);
                if tier != Tier::Banned && tier != Tier::Fatal {
                    continue;
                }
                if st.banned_until_ms.load(Ordering::Relaxed) == u64::MAX
                    && !probe.has_classify
                {
                    // permanent ban (give_up_route) and no classifier to lift it
                    continue;
                }
                let last = st.last_probe_ms.load(Ordering::Relaxed);
                if now.saturating_sub(last) < probe.interval_ms {
                    continue;
                }
                st.last_probe_ms.store(now, Ordering::Relaxed);

                let permit = match Arc::clone(&sem).acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let entry = Arc::clone(entry);
                let lua = Arc::clone(&lua);
                let route = Arc::clone(route);
                let probe_url = probe.url.clone();
                let ok_statuses = probe.ok_statuses.clone();
                let has_classify = probe.has_classify;
                let probe_headers = probe.headers.clone();
                let timeout = Duration::from_millis(route.timeout_ms);
                let cap = route.capture_body_bytes.max(if has_classify { 4096 } else { 0 });

                tokio::spawn(async move {
                    let _permit = permit;
                    match probe_request(&entry.upstream, &probe_url, timeout, cap, &probe_headers)
                        .await
                    {
                        Ok(reply) => {
                            let outcome = if has_classify {
                                lua.probe_classify(
                                    route.id,
                                    reply.status,
                                    &reply.headers,
                                    reply.body_preview.as_deref(),
                                    &ok_statuses,
                                )
                            } else if ok_statuses.contains(&reply.status) {
                                ProbeOutcome::Ok
                            } else {
                                ProbeOutcome::StillBanned
                            };

                            match outcome {
                                ProbeOutcome::Ok => {
                                    st.clear_ban();
                                    entry.clear_transport_fatal();
                                    info!(
                                        "probe[{}] {} restored ({})",
                                        route.name,
                                        sanitize_url(&entry.url),
                                        reply.status
                                    );
                                }
                                ProbeOutcome::StillBanned => {
                                    debug!(
                                        "probe[{}] {} still banned ({})",
                                        route.name,
                                        sanitize_url(&entry.url),
                                        reply.status
                                    );
                                }
                                ProbeOutcome::GiveUpRoute => {
                                    st.ban_permanent();
                                    warn!(
                                        "probe[{}] {} → give_up_route (permanent ban)",
                                        route.name,
                                        sanitize_url(&entry.url)
                                    );
                                }
                            }
                        }
                        Err(reason) => {
                            debug!(
                                "probe[{}] {} failed: {}",
                                route.name,
                                sanitize_url(&entry.url),
                                reason
                            );
                        }
                    }
                });
            }
        }
    }
}

/// Fast global recovery for proxies flagged `transport_fatal` (connect-class
/// dead). Independent of per-route probe intervals: a proxy that briefly lost
/// connectivity must come back in seconds, not after some route's 5–10 min
/// probe. Any HTTP response through it (regardless of status) proves the hop
/// is alive and clears the flag.
pub async fn run_transport_prober(
    pool: Arc<ProxyPool>,
    probe_url: String,
    interval: Duration,
    timeout: Duration,
    max_concurrent: usize,
) {
    if probe_url.is_empty() || interval.is_zero() {
        info!("transport prober disabled");
        return;
    }
    info!(
        "transport prober: every {}ms via {}",
        interval.as_millis(),
        probe_url
    );
    let sem = Arc::new(Semaphore::new(max_concurrent.max(1)));
    loop {
        tokio::time::sleep(interval).await;
        let snap = pool.snapshot();
        for entry in snap.iter() {
            if !entry.is_transport_fatal() {
                continue;
            }
            let permit = match Arc::clone(&sem).acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let entry = Arc::clone(entry);
            let url = probe_url.clone();
            tokio::spawn(async move {
                let _permit = permit;
                match probe_request(&entry.upstream, &url, timeout, 0, &[]).await {
                    Ok(reply) => {
                        if entry.clear_transport_fatal() {
                            info!(
                                "transport prober: {} reachable again ({}) → restored",
                                sanitize_url(&entry.url),
                                reply.status
                            );
                        }
                    }
                    Err(reason) => {
                        debug!(
                            "transport prober: {} still down: {}",
                            sanitize_url(&entry.url),
                            reason
                        );
                    }
                }
            });
        }
    }
}
