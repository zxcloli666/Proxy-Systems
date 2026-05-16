use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use mlua::{Lua, Table, Value};
use regex::Regex;

use super::matcher::{glob_to_regex, HostMatch, Matcher, PathMatch};
use crate::proxy::RouteId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorKind {
    BestLatency,
    RoundRobin,
    Random,
    Hedge,
}

#[derive(Debug, Clone)]
pub enum Selector {
    BestLatency,
    RoundRobin,
    Random,
    StickyByHeader(String),
    Hedge,
}

impl Selector {
    pub fn kind(&self) -> SelectorKind {
        match self {
            Selector::BestLatency => SelectorKind::BestLatency,
            Selector::RoundRobin => SelectorKind::RoundRobin,
            Selector::Random => SelectorKind::Random,
            Selector::StickyByHeader(_) => SelectorKind::BestLatency,
            Selector::Hedge => SelectorKind::Hedge,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HedgeCfg {
    pub max_parallel: usize,
    pub delay_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ProbeCfg {
    pub url: String,
    pub interval_ms: u64,
    pub ok_statuses: Vec<u16>,
    pub has_classify: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct HookFlags {
    pub on_request: bool,
    pub on_response: bool,
    pub on_exhausted: bool,
}

#[derive(Debug)]
pub struct RouteDef {
    pub id: RouteId,
    pub name: String,
    pub matcher: Matcher,
    pub pool_all_tags: Vec<String>,
    pub pool_any_tags: Vec<String>,
    /// Proxies carrying any of these tags are floated to the front of the
    /// attempt plan (still health-filtered: a banned preferred proxy is
    /// skipped, not forced).
    pub prefer_tags: Vec<String>,
    pub selector: Selector,
    pub hedge: HedgeCfg,
    pub max_attempts: usize,
    pub timeout_ms: u64,
    pub slow_ms: u64,
    pub fail_streak: u32,
    pub attempt_backoff_ms: u64,
    pub capture_body_bytes: usize,
    pub prefer_non_reserve: bool,
    pub default_ban_ms: u64,
    pub hooks: HookFlags,
    pub probe: Option<ProbeCfg>,
    pub cursor: AtomicUsize,
}

#[derive(Debug)]
pub struct RouterConfig {
    pub routes: Vec<Arc<RouteDef>>,
}

impl RouterConfig {
    /// First route whose static matcher accepts the request. Lua predicate is
    /// checked by the caller for routes that declare one.
    pub fn candidates(&self, host: &str, path: &str, method: &str) -> Vec<&Arc<RouteDef>> {
        self.routes
            .iter()
            .filter(|r| r.matcher.matches_static(host, path, method))
            .collect()
    }
}

struct Defaults {
    max_attempts: usize,
    timeout_ms: u64,
    slow_ms: u64,
    fail_streak: u32,
    attempt_backoff_ms: u64,
    capture_body_bytes: usize,
    prefer_non_reserve: bool,
    prefer_tags: Vec<String>,
    default_ban_ms: u64,
    selector: Selector,
    hedge: HedgeCfg,
}

fn get_num(t: &Table, key: &str) -> Option<f64> {
    match t.get::<Value>(key) {
        Ok(Value::Integer(i)) => Some(i as f64),
        Ok(Value::Number(n)) => Some(n),
        _ => None,
    }
}

fn get_u64(t: &Table, key: &str, default: u64) -> u64 {
    get_num(t, key).map(|n| n as u64).unwrap_or(default)
}

fn get_usize(t: &Table, key: &str, default: usize) -> usize {
    get_num(t, key).map(|n| n as usize).unwrap_or(default)
}

fn get_bool(t: &Table, key: &str, default: bool) -> bool {
    match t.get::<Value>(key) {
        Ok(Value::Boolean(b)) => b,
        _ => default,
    }
}

fn get_string(t: &Table, key: &str) -> Option<String> {
    match t.get::<Value>(key) {
        Ok(Value::String(s)) => s.to_str().ok().map(|s| s.to_string()),
        _ => None,
    }
}

fn get_string_list(t: &Table, key: &str) -> Vec<String> {
    match t.get::<Value>(key) {
        Ok(Value::Table(list)) => list
            .sequence_values::<String>()
            .filter_map(|v| v.ok())
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_selector(t: &Table, default: &Selector) -> Selector {
    match get_string(t, "selector").as_deref() {
        Some("best_latency") => Selector::BestLatency,
        Some("round_robin") => Selector::RoundRobin,
        Some("random") => Selector::Random,
        Some("hedge") => Selector::Hedge,
        Some("sticky_by_header") => {
            let h = get_string(t, "sticky_header").unwrap_or_else(|| "authorization".into());
            Selector::StickyByHeader(h.to_ascii_lowercase())
        }
        _ => default.clone(),
    }
}

fn parse_hedge(t: &Table, default: &HedgeCfg) -> HedgeCfg {
    match t.get::<Value>("hedge") {
        Ok(Value::Table(h)) => HedgeCfg {
            max_parallel: get_usize(&h, "max_parallel", default.max_parallel).max(1),
            delay_ms: get_u64(&h, "delay_ms", default.delay_ms),
        },
        _ => default.clone(),
    }
}

fn parse_matcher(route: &Table) -> Result<Matcher, String> {
    let m: Table = match route.get::<Value>("match") {
        Ok(Value::Table(t)) => t,
        _ => return Err("route.match missing or not a table".into()),
    };

    let host = if let Some(re) = get_string(&m, "host_regex") {
        HostMatch::Regex(Regex::new(&re).map_err(|e| format!("host_regex: {e}"))?)
    } else if let Some(h) = get_string(&m, "host") {
        if h == "*" {
            HostMatch::Any
        } else if let Some(rest) = h.strip_prefix("*.") {
            HostMatch::Suffix(format!(".{rest}"))
        } else {
            HostMatch::Exact(h)
        }
    } else {
        HostMatch::Any
    };

    let path = if let Some(re) = get_string(&m, "path_regex") {
        PathMatch::Regex(Regex::new(&re).map_err(|e| format!("path_regex: {e}"))?)
    } else if let Some(g) = get_string(&m, "path") {
        if g == "*" || g == "/**" {
            PathMatch::Any
        } else if g.ends_with("/**") && !g[..g.len() - 3].contains(['*', '?']) {
            PathMatch::Prefix(g[..g.len() - 3].to_string())
        } else {
            PathMatch::Glob(glob_to_regex(&g).map_err(|e| format!("path glob: {e}"))?)
        }
    } else {
        PathMatch::Any
    };

    let methods = match m.get::<Value>("methods") {
        Ok(Value::Table(list)) => {
            let v: Vec<String> = list
                .sequence_values::<String>()
                .filter_map(|x| x.ok())
                .collect();
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        }
        _ => None,
    };

    let has_predicate = matches!(m.get::<Value>("predicate"), Ok(Value::Function(_)));

    Ok(Matcher {
        host,
        path,
        methods,
        has_predicate,
    })
}

fn parse_defaults(cfg: &Table) -> Defaults {
    let d: Table = match cfg.get::<Value>("defaults") {
        Ok(Value::Table(t)) => t,
        _ => {
            // synthesize an empty table so get_* fall through to literals
            return Defaults {
                max_attempts: 5,
                timeout_ms: 10_000,
                slow_ms: 3_000,
                fail_streak: 2,
                attempt_backoff_ms: 0,
                capture_body_bytes: 0,
                prefer_non_reserve: true,
                prefer_tags: Vec::new(),
                default_ban_ms: 5 * 60_000,
                selector: Selector::BestLatency,
                hedge: HedgeCfg {
                    max_parallel: 4,
                    delay_ms: 700,
                },
            };
        }
    };
    let base_hedge = HedgeCfg {
        max_parallel: 4,
        delay_ms: 700,
    };
    Defaults {
        max_attempts: get_usize(&d, "max_attempts", 5).max(1),
        timeout_ms: get_u64(&d, "timeout_ms", 10_000),
        slow_ms: get_u64(&d, "slow_ms", 3_000),
        fail_streak: get_u64(&d, "fail_streak", 2) as u32,
        attempt_backoff_ms: get_u64(&d, "attempt_backoff_ms", 0),
        capture_body_bytes: get_usize(&d, "capture_body_bytes", 0),
        prefer_non_reserve: get_bool(&d, "prefer_non_reserve", true),
        prefer_tags: get_string_list(&d, "prefer_tags"),
        default_ban_ms: get_u64(&d, "default_ban_ms", 5 * 60_000),
        selector: parse_selector(&d, &Selector::BestLatency),
        hedge: parse_hedge(&d, &base_hedge),
    }
}

fn parse_probe(route: &Table) -> Option<ProbeCfg> {
    let p: Table = match route.get::<Value>("probe") {
        Ok(Value::Table(t)) => t,
        _ => return None,
    };
    let url = get_string(&p, "url")?;
    let ok_statuses: Vec<u16> = match p.get::<Value>("ok_statuses") {
        Ok(Value::Table(list)) => list
            .sequence_values::<i64>()
            .filter_map(|v| v.ok())
            .map(|n| n as u16)
            .collect(),
        _ => vec![200],
    };
    let has_classify = matches!(p.get::<Value>("classify"), Ok(Value::Function(_)));
    Some(ProbeCfg {
        url,
        interval_ms: get_u64(&p, "interval_ms", 10 * 60_000),
        ok_statuses,
        has_classify,
    })
}

/// Parse the table returned by `routes.lua` into a `RouterConfig`. Lua
/// functions stay in the VM; we only record which hooks exist (by flag) and
/// each route's 1-based index, used to address the hook from any pooled VM.
pub fn parse_router_config(_lua: &Lua, cfg: &Table) -> Result<RouterConfig, String> {
    let defaults = parse_defaults(cfg);
    let routes_tbl: Table = match cfg.get::<Value>("routes") {
        Ok(Value::Table(t)) => t,
        _ => return Err("config.routes missing or not a table".into()),
    };

    let mut routes = Vec::new();
    for (i, rv) in routes_tbl.sequence_values::<Table>().enumerate() {
        let idx = (i + 1) as u32;
        let route = rv.map_err(|e| format!("route #{idx}: {e}"))?;
        let name = get_string(&route, "name").unwrap_or_else(|| format!("route-{idx}"));
        let matcher = parse_matcher(&route).map_err(|e| format!("route '{name}': {e}"))?;

        let pool_all_tags;
        let pool_any_tags;
        match route.get::<Value>("pool") {
            Ok(Value::Table(p)) => {
                pool_all_tags = get_string_list(&p, "tags");
                pool_any_tags = get_string_list(&p, "any_tags");
            }
            _ => {
                pool_all_tags = Vec::new();
                pool_any_tags = Vec::new();
            }
        }

        let prefer_tags = match route.get::<Value>("prefer_tags") {
            Ok(Value::Table(_)) => get_string_list(&route, "prefer_tags"),
            _ => defaults.prefer_tags.clone(),
        };

        let selector = parse_selector(&route, &defaults.selector);
        let hedge = parse_hedge(&route, &defaults.hedge);

        let hooks = HookFlags {
            on_request: matches!(route.get::<Value>("on_request"), Ok(Value::Function(_))),
            on_response: matches!(route.get::<Value>("on_response"), Ok(Value::Function(_))),
            on_exhausted: matches!(route.get::<Value>("on_exhausted"), Ok(Value::Function(_))),
        };

        routes.push(Arc::new(RouteDef {
            id: idx,
            name,
            matcher,
            pool_all_tags,
            pool_any_tags,
            prefer_tags,
            selector,
            hedge,
            max_attempts: get_usize(&route, "max_attempts", defaults.max_attempts).max(1),
            timeout_ms: get_u64(&route, "timeout_ms", defaults.timeout_ms),
            slow_ms: get_u64(&route, "slow_ms", defaults.slow_ms),
            fail_streak: get_u64(&route, "fail_streak", defaults.fail_streak as u64) as u32,
            attempt_backoff_ms: get_u64(
                &route,
                "attempt_backoff_ms",
                defaults.attempt_backoff_ms,
            ),
            capture_body_bytes: get_usize(
                &route,
                "capture_body_bytes",
                defaults.capture_body_bytes,
            ),
            prefer_non_reserve: get_bool(
                &route,
                "prefer_non_reserve",
                defaults.prefer_non_reserve,
            ),
            default_ban_ms: get_u64(&route, "default_ban_ms", defaults.default_ban_ms),
            hooks,
            probe: parse_probe(&route),
            cursor: AtomicUsize::new(0),
        }));
    }

    if routes.is_empty() {
        return Err("config.routes is empty".into());
    }
    Ok(RouterConfig { routes })
}
