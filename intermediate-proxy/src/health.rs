use axum::extract::{Path, Query, State};
use axum::response::Response;
use proxy_common::response::json_response;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::route::state::entry_tier;
use crate::util::{now_ms, sanitize_url};
use crate::AppState;

fn tier_counts(state: &AppState) -> serde_json::Value {
    let cfg = state.lua.config();
    let snap = state.pool.snapshot();
    let now = now_ms();
    let mut routes = Vec::new();
    for r in &cfg.routes {
        let mut c = [0u64; 5];
        for e in snap.iter() {
            let st = e.route_state(r.id);
            c[entry_tier(e, &st, r, now) as usize] += 1;
        }
        routes.push(serde_json::json!({
            "name": r.name,
            "selector": format!("{:?}", r.selector),
            "healthy": c[0],
            "slow": c[1],
            "failed": c[2],
            "banned": c[3],
            "fatal": c[4],
            "probe": r.probe.as_ref().map(|p| serde_json::json!({
                "url": p.url,
                "intervalMs": p.interval_ms,
                "okStatuses": p.ok_statuses,
            })),
        }));
    }
    serde_json::Value::Array(routes)
}

pub async fn health_handler(State(state): State<Arc<AppState>>) -> Response {
    let body = serde_json::json!({
        "status": "ok",
        "proxies": state.pool.summary(),
        "routes": tier_counts(&state),
    });
    json_response(axum::http::StatusCode::OK, &body.to_string())
}

pub async fn route_detail_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let cfg = state.lua.config();
    let Some(route) = cfg.routes.iter().find(|r| r.name == name) else {
        return json_response(
            axum::http::StatusCode::NOT_FOUND,
            &serde_json::json!({ "error": "no such route", "name": name }).to_string(),
        );
    };
    let snap = state.pool.snapshot();
    let now = now_ms();
    let mut proxies = Vec::new();
    for e in snap.iter() {
        let st = e.route_state(route.id);
        let tier = entry_tier(e, &st, route, now);
        proxies.push(serde_json::json!({
            "url": sanitize_url(&e.url),
            "tags": e.tags().as_ref().clone(),
            "tier": tier.as_str(),
            "avgLatencyMs": st.avg_latency_ms.load(Ordering::Relaxed),
            "lastLatencyMs": st.last_latency_ms.load(Ordering::Relaxed),
            "successCount": st.success_count.load(Ordering::Relaxed),
            "errorCount": st.error_count.load(Ordering::Relaxed),
            "consecutiveFailures": st.consecutive_failures.load(Ordering::Relaxed),
            "bannedUntilMs": st.banned_until_ms.load(Ordering::Relaxed),
            "banCount": st.ban_count.load(Ordering::Relaxed),
            "lastProbeMs": st.last_probe_ms.load(Ordering::Relaxed),
            "lastErrorReason": st.last_error_reason.lock().clone(),
            "transportFatal": e.is_transport_fatal(),
        }));
    }
    let body = serde_json::json!({
        "route": route.name,
        "proxies": proxies,
    });
    json_response(axum::http::StatusCode::OK, &body.to_string())
}

pub async fn proxy_detail_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(url) = q.get("url") else {
        return json_response(
            axum::http::StatusCode::BAD_REQUEST,
            &serde_json::json!({ "error": "missing ?url=" }).to_string(),
        );
    };
    let Some(entry) = state.pool.find(url) else {
        return json_response(
            axum::http::StatusCode::NOT_FOUND,
            &serde_json::json!({ "error": "no such proxy" }).to_string(),
        );
    };
    let cfg = state.lua.config();
    let now = now_ms();
    let mut routes = Vec::new();
    for r in &cfg.routes {
        let st = entry.route_state(r.id);
        if st.success_count.load(Ordering::Relaxed) == 0
            && st.error_count.load(Ordering::Relaxed) == 0
        {
            continue;
        }
        routes.push(serde_json::json!({
            "route": r.name,
            "tier": entry_tier(&entry, &st, r, now).as_str(),
            "avgLatencyMs": st.avg_latency_ms.load(Ordering::Relaxed),
            "successCount": st.success_count.load(Ordering::Relaxed),
            "errorCount": st.error_count.load(Ordering::Relaxed),
            "bannedUntilMs": st.banned_until_ms.load(Ordering::Relaxed),
            "lastErrorReason": st.last_error_reason.lock().clone(),
        }));
    }
    let body = serde_json::json!({
        "proxy": sanitize_url(&entry.url),
        "tags": entry.tags().as_ref().clone(),
        "transportFatal": entry.is_transport_fatal(),
        "globalSuccess": entry.global.success_count.load(Ordering::Relaxed),
        "globalError": entry.global.error_count.load(Ordering::Relaxed),
        "routes": routes,
    });
    json_response(axum::http::StatusCode::OK, &body.to_string())
}
