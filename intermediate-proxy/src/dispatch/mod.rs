use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use proxy_common::cors::cors_headers;
use proxy_common::response::text_response;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, warn};

use crate::forward::{describe_reqwest_error, send_to_upstream};
use crate::lua::{ExhaustedVerdict, HookCtx, HookRes, LuaPool, RequestMods, ResponseVerdict};
use crate::proxy::{is_transport_fatal_reason, Entry, ProxyPool};
use crate::route::config::{RouteDef, Selector};
use crate::route::state::{build_plan, fallback_pool, Candidate};
use crate::stream::{
    buffered_response, build_streaming_response, can_recover_stream, is_html_response,
};
use crate::util::sanitize_url;

const MAX_CAPTURE_BUFFER: usize = 10 * 1024 * 1024;
const FAILED_BODY_CAP: usize = 64 * 1024;

struct FailedResponse {
    status: StatusCode,
    content_type: Option<axum::http::HeaderValue>,
    body: Bytes,
}

pub struct Request {
    pub method: Method,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub target_url: String,
    pub host: String,
    pub path: String,
}

fn header_pairs(h: &HeaderMap) -> Vec<(String, String)> {
    let mut v = Vec::with_capacity(h.len());
    for (k, val) in h.iter() {
        if let Ok(s) = val.to_str() {
            v.push((k.as_str().to_ascii_lowercase(), s.to_string()));
        }
    }
    v
}

fn resp_header_pairs(h: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    let mut v = Vec::with_capacity(h.len());
    for (k, val) in h.iter() {
        if let Ok(s) = val.to_str() {
            v.push((k.as_str().to_ascii_lowercase(), s.to_string()));
        }
    }
    v
}

fn base_ctx(route: &RouteDef, req: &Request, hdr_pairs: &[(String, String)]) -> HookCtx {
    HookCtx {
        route_name: route.name.clone(),
        target_url: req.target_url.clone(),
        host: req.host.clone(),
        path: req.path.clone(),
        method: req.method.to_string(),
        headers: hdr_pairs.to_vec(),
        attempt: 1,
        proxy_url: None,
        proxy_tags: Vec::new(),
        elapsed_ms: 0,
    }
}

fn default_verdict(status: u16, default_ban_ms: u64) -> ResponseVerdict {
    match status {
        403 => ResponseVerdict::ReturnAsIs,
        421 | 429 | 500 | 502 | 503 => ResponseVerdict::RetryOtherProxy {
            ban_for_ms: default_ban_ms,
        },
        s if s >= 500 => ResponseVerdict::RetryOtherProxy {
            ban_for_ms: default_ban_ms,
        },
        _ => ResponseVerdict::Success,
    }
}

fn apply_request_mods(headers: &mut HeaderMap, mods: &RequestMods) {
    for d in &mods.drop_headers {
        headers.remove(d.as_str());
    }
    for (k, v) in &mods.add_headers {
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::from_bytes(k.as_bytes()),
            axum::http::HeaderValue::from_str(v),
        ) {
            headers.insert(name, val);
        }
    }
}

async fn buffer_failed(resp: reqwest::Response) -> FailedResponse {
    let status =
        StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| axum::http::HeaderValue::from_bytes(v.as_bytes()).ok());
    let mut buf: Vec<u8> = Vec::new();
    let mut s = resp.bytes_stream();
    while let Some(c) = s.next().await {
        match c {
            Ok(b) => {
                let rem = FAILED_BODY_CAP.saturating_sub(buf.len());
                if rem == 0 {
                    break;
                }
                if b.len() <= rem {
                    buf.extend_from_slice(&b);
                } else {
                    buf.extend_from_slice(&b[..rem]);
                    break;
                }
            }
            Err(_) => break,
        }
    }
    FailedResponse {
        status,
        content_type,
        body: Bytes::from(buf),
    }
}

fn build_most_common(failures: Vec<FailedResponse>) -> Option<Response> {
    if failures.is_empty() {
        return None;
    }
    let mut counts: HashMap<u16, usize> = HashMap::new();
    for f in &failures {
        *counts.entry(f.status.as_u16()).or_insert(0) += 1;
    }
    let best = counts.iter().max_by_key(|(_, c)| *c).map(|(s, _)| *s)?;
    let pick = failures.into_iter().find(|f| f.status.as_u16() == best)?;
    let mut builder = Response::builder().status(pick.status);
    let hdrs = builder.headers_mut().unwrap();
    for (n, v) in cors_headers() {
        hdrs.insert(n, v);
    }
    if let Some(ct) = pick.content_type {
        hdrs.insert("content-type", ct);
    }
    builder.body(Body::from(pick.body)).ok()
}

fn record_success(entry: &Entry, route_id: u32, latency: u64) {
    let st = entry.route_state(route_id);
    st.observe_success(latency);
    st.clear_ban();
    entry.global.usage_count.fetch_add(1, Ordering::Relaxed);
    entry.global.success_count.fetch_add(1, Ordering::Relaxed);
    entry
        .global
        .last_success_ms
        .store(crate::util::now_ms(), Ordering::Relaxed);
}

fn record_failure(
    entry: &Entry,
    route: &RouteDef,
    latency: Option<u64>,
    reason: &str,
    ban_ms: Option<u64>,
    transport: bool,
) {
    let st = entry.route_state(route.id);
    st.observe_failure(latency, reason, route.slow_ms);
    if let Some(b) = ban_ms {
        st.ban_for(b);
    }
    entry.global.usage_count.fetch_add(1, Ordering::Relaxed);
    entry.global.error_count.fetch_add(1, Ordering::Relaxed);
    entry
        .global
        .last_error_ms
        .store(crate::util::now_ms(), Ordering::Relaxed);
    if transport && is_transport_fatal_reason(reason) && entry.mark_transport_fatal() {
        warn!(
            "marking {} transport-fatal: {}",
            sanitize_url(&entry.url),
            reason
        );
    }
}

enum AttemptOk {
    Stream {
        resp: reqwest::Response,
        url: String,
        recover: bool,
    },
    Buffered {
        status: StatusCode,
        headers: reqwest::header::HeaderMap,
        body: Bytes,
    },
    GiveUp {
        status: u16,
        body: String,
    },
}

enum AttemptErr {
    Next { failed: Option<FailedResponse> },
}

#[allow(clippy::too_many_arguments)]
async fn run_attempt(
    lua: &Arc<LuaPool>,
    route: &Arc<RouteDef>,
    cand: &Candidate,
    req: &Request,
    hdr_pairs: &[(String, String)],
    base_headers: &HeaderMap,
    attempt: u32,
    started: Instant,
) -> Result<AttemptOk, AttemptErr> {
    let entry = &cand.entry;
    let timeout = Duration::from_millis(route.timeout_ms);
    let start = Instant::now();
    let result = tokio::time::timeout(
        timeout,
        send_to_upstream(&entry.upstream, &req.method, base_headers, req.body.clone(), None),
    )
    .await;
    let elapsed = start.elapsed().as_millis() as u64;

    let resp = match result {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            let reason = describe_reqwest_error(&e);
            warn!(
                "upstream {} error in {}ms: {}",
                sanitize_url(&entry.url),
                elapsed,
                reason
            );
            record_failure(entry, route, None, &reason, None, true);
            return Err(AttemptErr::Next { failed: None });
        }
        Err(_) => {
            let reason = format!("timeout {}ms", timeout.as_millis());
            warn!("upstream {} {}", sanitize_url(&entry.url), reason);
            record_failure(entry, route, None, &reason, None, true);
            return Err(AttemptErr::Next { failed: None });
        }
    };

    let status = resp.status().as_u16();
    let ctx_for = |a: u32| HookCtx {
        attempt: a,
        proxy_url: Some(sanitize_url(&entry.url)),
        proxy_tags: entry.tags().as_ref().clone(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        ..base_ctx(route, req, hdr_pairs)
    };

    // Capture path: route opted into body inspection, so we buffer the whole
    // body. Streaming + mid-stream recovery are unavailable here by design.
    if route.capture_body_bytes > 0 {
        let rheaders = resp.headers().clone();
        let full = match resp.bytes().await {
            Ok(b) if b.len() > MAX_CAPTURE_BUFFER => b.slice(0..MAX_CAPTURE_BUFFER),
            Ok(b) => b,
            Err(e) => {
                let reason = describe_reqwest_error(&e);
                record_failure(entry, route, Some(elapsed), &reason, None, true);
                return Err(AttemptErr::Next { failed: None });
            }
        };
        let take = full.len().min(route.capture_body_bytes);
        let preview = String::from_utf8_lossy(&full[..take]).into_owned();
        let verdict = if route.hooks.on_response {
            let hr = HookRes {
                status,
                headers: resp_header_pairs(&rheaders),
                body_preview: Some(preview),
                elapsed_ms: elapsed,
                bytes_received: full.len() as u64,
            };
            lua.on_response(route.id, &ctx_for(attempt), &hr, route.default_ban_ms)
        } else {
            default_verdict(status, route.default_ban_ms)
        };
        return match verdict {
            ResponseVerdict::Success | ResponseVerdict::ReturnAsIs => {
                record_success(entry, route.id, elapsed);
                Ok(AttemptOk::Buffered {
                    status: StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
                    headers: rheaders,
                    body: full,
                })
            }
            ResponseVerdict::RetryOtherProxy { ban_for_ms } => {
                record_failure(
                    entry,
                    route,
                    Some(elapsed),
                    &format!("status {status}"),
                    Some(ban_for_ms),
                    false,
                );
                let failed = if is_html_ct(&rheaders) {
                    None
                } else {
                    Some(FailedResponse {
                        status: StatusCode::from_u16(status)
                            .unwrap_or(StatusCode::BAD_GATEWAY),
                        content_type: rheaders
                            .get("content-type")
                            .and_then(|v| {
                                axum::http::HeaderValue::from_bytes(v.as_bytes()).ok()
                            }),
                        body: full,
                    })
                };
                Err(AttemptErr::Next { failed })
            }
            ResponseVerdict::HardFail { reason } => {
                record_failure(entry, route, Some(elapsed), &reason, None, true);
                Err(AttemptErr::Next { failed: None })
            }
            ResponseVerdict::RetrySameProxy { delay_ms } => {
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms.min(5000))).await;
                }
                record_failure(
                    entry,
                    route,
                    Some(elapsed),
                    &format!("retry_same status {status}"),
                    None,
                    false,
                );
                Err(AttemptErr::Next { failed: None })
            }
            ResponseVerdict::GiveUp { status, body } => {
                Ok(AttemptOk::GiveUp { status, body })
            }
        };
    }

    // Streaming path: Lua only sees status + headers (no body read), so the
    // response stays streamable and mid-stream recovery stays available.
    let verdict = if route.hooks.on_response {
        let hr = HookRes {
            status,
            headers: resp_header_pairs(resp.headers()),
            body_preview: None,
            elapsed_ms: elapsed,
            bytes_received: 0,
        };
        lua.on_response(route.id, &ctx_for(attempt), &hr, route.default_ban_ms)
    } else {
        default_verdict(status, route.default_ban_ms)
    };

    match verdict {
        ResponseVerdict::Success => {
            let recover = req.method == Method::GET && can_recover_stream(base_headers, &resp);
            record_success(entry, route.id, elapsed);
            debug!(
                "upstream {} ok {} in {}ms",
                sanitize_url(&entry.url),
                status,
                elapsed
            );
            Ok(AttemptOk::Stream {
                resp,
                url: entry.url.clone(),
                recover,
            })
        }
        ResponseVerdict::ReturnAsIs => {
            record_success(entry, route.id, elapsed);
            Ok(AttemptOk::Stream {
                resp,
                url: entry.url.clone(),
                recover: false,
            })
        }
        ResponseVerdict::RetryOtherProxy { ban_for_ms } => {
            record_failure(
                entry,
                route,
                Some(elapsed),
                &format!("status {status}"),
                Some(ban_for_ms),
                false,
            );
            let failed = if is_html_response(resp.headers()) {
                None
            } else {
                Some(buffer_failed(resp).await)
            };
            Err(AttemptErr::Next { failed })
        }
        ResponseVerdict::HardFail { reason } => {
            record_failure(entry, route, Some(elapsed), &reason, None, true);
            Err(AttemptErr::Next { failed: None })
        }
        ResponseVerdict::RetrySameProxy { delay_ms } => {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms.min(5000))).await;
            }
            record_failure(
                entry,
                route,
                Some(elapsed),
                &format!("retry_same status {status}"),
                None,
                false,
            );
            Err(AttemptErr::Next { failed: None })
        }
        ResponseVerdict::GiveUp { status, body } => Ok(AttemptOk::GiveUp { status, body }),
    }
}

fn is_html_ct(h: &reqwest::header::HeaderMap) -> bool {
    h.get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            let l = ct.to_ascii_lowercase();
            l.starts_with("text/html") || l.starts_with("application/xhtml")
        })
        .unwrap_or(false)
}

fn finalize_ok(
    ok: AttemptOk,
    pool: &Arc<ProxyPool>,
    route: &Arc<RouteDef>,
    req: &Request,
    base_headers: &HeaderMap,
    tried: HashSet<String>,
) -> Pin<Box<dyn Future<Output = Response> + Send>> {
    let pool = Arc::clone(pool);
    let route = Arc::clone(route);
    let method = req.method.clone();
    let body = req.body.clone();
    let headers = base_headers.clone();
    Box::pin(async move {
        match ok {
            AttemptOk::Buffered {
                status,
                headers: rh,
                body,
            } => buffered_response(status, &rh, body),
            AttemptOk::GiveUp { status, body } => text_response(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                &body,
            ),
            AttemptOk::Stream { resp, url, recover } => {
                build_streaming_response(
                    resp, pool, route, url, &method, &headers, &body, recover, tried,
                )
                .await
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_sequential(
    pool: Arc<ProxyPool>,
    lua: Arc<LuaPool>,
    route: Arc<RouteDef>,
    req: Request,
    hdr_pairs: Vec<(String, String)>,
    base_headers: HeaderMap,
    plan: Vec<Candidate>,
    tried: &mut HashSet<String>,
    failures: &mut Vec<FailedResponse>,
) -> Option<Response> {
    let started = Instant::now();
    let mut attempt = 0u32;
    for cand in &plan {
        tried.insert(cand.entry.url.clone());
        attempt += 1;
        if route.attempt_backoff_ms > 0 && attempt > 1 {
            tokio::time::sleep(Duration::from_millis(route.attempt_backoff_ms)).await;
        }
        match run_attempt(
            &lua,
            &route,
            cand,
            &req,
            &hdr_pairs,
            &base_headers,
            attempt,
            started,
        )
        .await
        {
            Ok(ok) => {
                let t = tried.clone();
                return Some(finalize_ok(ok, &pool, &route, &req, &base_headers, t).await);
            }
            Err(AttemptErr::Next { failed }) => {
                if let Some(f) = failed {
                    failures.push(f);
                }
            }
        }
    }
    None
}

type AttemptFut = Pin<Box<dyn Future<Output = (usize, Result<AttemptOk, AttemptErr>)> + Send>>;

#[allow(clippy::too_many_arguments)]
async fn dispatch_hedged(
    pool: Arc<ProxyPool>,
    lua: Arc<LuaPool>,
    route: Arc<RouteDef>,
    req: Arc<Request>,
    hdr_pairs: Arc<Vec<(String, String)>>,
    base_headers: Arc<HeaderMap>,
    plan: Arc<Vec<Candidate>>,
    tried: &mut HashSet<String>,
    failures: &mut Vec<FailedResponse>,
) -> Option<Response> {
    let started = Instant::now();
    let max_par = route.hedge.max_parallel.max(1);
    let delay = Duration::from_millis(route.hedge.delay_ms);
    let mut futs: FuturesUnordered<AttemptFut> = FuturesUnordered::new();
    let mut next = 0usize;

    let mut launch = |futs: &mut FuturesUnordered<AttemptFut>, next: &mut usize| -> bool {
        while *next < plan.len() {
            let idx = *next;
            *next += 1;
            let cand_url = plan[idx].entry.url.clone();
            if !tried.insert(cand_url) {
                continue;
            }
            let lua = Arc::clone(&lua);
            let route = Arc::clone(&route);
            let req = Arc::clone(&req);
            let hp = Arc::clone(&hdr_pairs);
            let bh = Arc::clone(&base_headers);
            let plan = Arc::clone(&plan);
            futs.push(Box::pin(async move {
                let r = run_attempt(
                    &lua,
                    &route,
                    &plan[idx],
                    &req,
                    &hp,
                    &bh,
                    (idx + 1) as u32,
                    started,
                )
                .await;
                (idx, r)
            }));
            return true;
        }
        false
    };

    launch(&mut futs, &mut next);
    let timer = tokio::time::sleep(delay);
    tokio::pin!(timer);

    while !futs.is_empty() {
        tokio::select! {
            biased;
            Some((_, res)) = futs.next() => {
                match res {
                    Ok(ok) => {
                        let t = tried.clone();
                        drop(futs);
                        return Some(finalize_ok(ok, &pool, &route, &req, &base_headers, t).await);
                    }
                    Err(AttemptErr::Next { failed }) => {
                        if let Some(f) = failed { failures.push(f); }
                        launch(&mut futs, &mut next);
                    }
                }
            }
            _ = &mut timer => {
                timer.as_mut().reset(tokio::time::Instant::now() + delay);
                if futs.len() < max_par {
                    launch(&mut futs, &mut next);
                }
            }
        }
    }
    None
}

/// Entry point: route already selected. Runs the route's selector/retry policy
/// and, on exhaustion, the route's `on_exhausted` hook.
pub async fn run(
    pool: Arc<ProxyPool>,
    lua: Arc<LuaPool>,
    route: Arc<RouteDef>,
    req: Request,
    max_hard_attempts: usize,
) -> Response {
    let hdr_pairs = header_pairs(&req.headers);

    // on_request runs once before dispatch; it can rewrite headers and
    // override the proxy pool / force a proxy for this request.
    let mut base_headers = req.headers.clone();
    let mut pool_all = route.pool_all_tags.clone();
    let mut pool_any = route.pool_any_tags.clone();
    let mut forced: Option<String> = None;
    if route.hooks.on_request {
        let ctx = base_ctx(&route, &req, &hdr_pairs);
        let mods = lua.on_request(route.id, &ctx);
        apply_request_mods(&mut base_headers, &mods);
        if let Some(tags) = mods.override_all_tags {
            pool_all = tags;
            pool_any = Vec::new();
        }
        forced = mods.force_proxy;
    }

    let sticky_key = match &route.selector {
        Selector::StickyByHeader(h) => base_headers
            .get(h.as_str())
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
        _ => None,
    };

    let mut tried: HashSet<String> = HashSet::new();
    let mut failures: Vec<FailedResponse> = Vec::new();
    let mut hard_rounds = 0usize;

    loop {
        let snapshot = pool.snapshot();
        let mut plan = build_plan(
            &route,
            &snapshot,
            &pool_all,
            &pool_any,
            sticky_key.as_deref(),
        );

        if let Some(furl) = &forced {
            if let Some(e) = pool.find(furl) {
                let lat = e
                    .route_state(route.id)
                    .avg_latency_ms
                    .load(Ordering::Relaxed);
                plan.insert(
                    0,
                    Candidate {
                        entry: e,
                        latency: lat,
                        tier: crate::route::state::Tier::Healthy,
                    },
                );
            }
            forced = None;
        }

        if !plan.is_empty() {
            let result = if matches!(route.selector, Selector::Hedge) {
                let req_a = Arc::new(Request {
                    method: req.method.clone(),
                    headers: req.headers.clone(),
                    body: req.body.clone(),
                    target_url: req.target_url.clone(),
                    host: req.host.clone(),
                    path: req.path.clone(),
                });
                dispatch_hedged(
                    Arc::clone(&pool),
                    Arc::clone(&lua),
                    Arc::clone(&route),
                    req_a,
                    Arc::new(hdr_pairs.clone()),
                    Arc::new(base_headers.clone()),
                    Arc::new(plan),
                    &mut tried,
                    &mut failures,
                )
                .await
            } else {
                dispatch_sequential(
                    Arc::clone(&pool),
                    Arc::clone(&lua),
                    Arc::clone(&route),
                    Request {
                        method: req.method.clone(),
                        headers: req.headers.clone(),
                        body: req.body.clone(),
                        target_url: req.target_url.clone(),
                        host: req.host.clone(),
                        path: req.path.clone(),
                    },
                    hdr_pairs.clone(),
                    base_headers.clone(),
                    plan,
                    &mut tried,
                    &mut failures,
                )
                .await
            };
            if let Some(resp) = result {
                return resp;
            }
        }

        // Exhausted: ask the route what to do.
        let ctx = HookCtx {
            attempt: (tried.len() + 1) as u32,
            elapsed_ms: 0,
            ..base_ctx(&route, &req, &hdr_pairs)
        };
        let verdict = if route.hooks.on_exhausted {
            lua.on_exhausted(route.id, &ctx)
        } else {
            ExhaustedVerdict::TryFatal
        };

        match verdict {
            ExhaustedVerdict::TryFatal => {
                if let Some(resp) = try_fatal(
                    &pool,
                    &route,
                    &req,
                    &base_headers,
                    &pool_all,
                    &pool_any,
                    &tried,
                    &mut failures,
                )
                .await
                {
                    return resp;
                }
                break;
            }
            ExhaustedVerdict::WaitProbe { timeout_ms } => {
                tokio::time::sleep(Duration::from_millis(timeout_ms.min(30_000))).await;
                hard_rounds += 1;
                if hard_rounds >= max_hard_attempts {
                    break;
                }
                continue;
            }
            ExhaustedVerdict::RetryAll => {
                hard_rounds += 1;
                if hard_rounds >= max_hard_attempts {
                    break;
                }
                tried.clear();
                continue;
            }
            ExhaustedVerdict::ReturnError { status, body } => {
                return text_response(
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    &body,
                );
            }
        }
    }

    error!(
        "route '{}' exhausted all proxies ({} tried)",
        route.name,
        tried.len()
    );
    if let Some(resp) = build_most_common(failures) {
        return resp;
    }
    text_response(
        StatusCode::BAD_GATEWAY,
        &format!("Proxy Error: route '{}' exhausted", route.name),
    )
}

#[allow(clippy::too_many_arguments)]
async fn try_fatal(
    pool: &Arc<ProxyPool>,
    route: &Arc<RouteDef>,
    req: &Request,
    base_headers: &HeaderMap,
    pool_all: &[String],
    pool_any: &[String],
    tried: &HashSet<String>,
    failures: &mut Vec<FailedResponse>,
) -> Option<Response> {
    let snapshot = pool.snapshot();
    let fb = fallback_pool(route, &snapshot, pool_all, pool_any, tried);
    let timeout = Duration::from_millis(route.timeout_ms);
    for entry in fb {
        warn!(
            "fatal fallback: trying {} (route '{}')",
            sanitize_url(&entry.url),
            route.name
        );
        let result = tokio::time::timeout(
            timeout,
            send_to_upstream(
                &entry.upstream,
                &req.method,
                base_headers,
                req.body.clone(),
                None,
            ),
        )
        .await;
        match result {
            Ok(Ok(resp)) => {
                let status = resp.status().as_u16();
                if (500..=599).contains(&status) || status == 429 || status == 421 {
                    if !is_html_response(resp.headers()) {
                        failures.push(buffer_failed(resp).await);
                    }
                    continue;
                }
                if status == 403 {
                    if !is_html_response(resp.headers()) {
                        failures.push(buffer_failed(resp).await);
                    }
                    continue;
                }
                return Some(
                    build_streaming_response(
                        resp,
                        Arc::clone(pool),
                        Arc::clone(route),
                        entry.url.clone(),
                        &req.method,
                        base_headers,
                        &req.body,
                        false,
                        tried.clone(),
                    )
                    .await,
                );
            }
            Ok(Err(_)) | Err(_) => continue,
        }
    }
    None
}
