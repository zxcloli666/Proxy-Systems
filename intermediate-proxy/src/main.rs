mod dispatch;
mod forward;
mod handler;
mod health;
mod lua;
mod proxy;
mod route;
mod stream;
mod tls;
mod upstream;
mod util;

use axum::routing::{any, get};
use axum::Router;
use proxy_common::cors::cors_layer;
use proxy_common::server::{bind_tcp, init_tracing, port_from_env};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use crate::lua::LuaPool;
use crate::proxy::ProxyPool;
use crate::util::{env_bool, env_string, env_u64};

pub struct AppState {
    pub pool: Arc<ProxyPool>,
    pub lua: Arc<LuaPool>,
    pub max_hard_attempts: usize,
}

#[tokio::main]
async fn main() {
    init_tracing("info");

    let proxy_file = env_string("PROXY_FILE", "/etc/proxies.txt");
    let proxy_refresh_ms = env_u64("PROXY_REFRESH_MS", 30_000);
    let routes_file = env_string("ROUTES_FILE", "/etc/routes.lua");
    let routes_refresh_ms = env_u64("ROUTES_REFRESH_MS", 30_000);
    let max_concurrent_probes = env_u64("MAX_CONCURRENT_PROBES", 16) as usize;
    let instr_limit = env_u64("LUA_INSTRUCTION_LIMIT", 1_000_000) as u32;
    let max_hard_attempts = env_u64("MAX_HARD_ATTEMPTS", 20) as usize;
    let transport_probe_url =
        env_string("TRANSPORT_PROBE_URL", "https://www.google.com/generate_204");
    let transport_probe_interval_ms = env_u64("TRANSPORT_PROBE_INTERVAL_MS", 10_000);
    let transport_probe_timeout_ms = env_u64("TRANSPORT_PROBE_TIMEOUT_MS", 8_000);

    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let vm_count = env_u64("LUA_VM_COUNT", (cpus * 2).max(4) as u64) as usize;

    let pool = ProxyPool::new(proxy::load_initial(&proxy_file));

    let lua = match lua::build_from_file(&routes_file, vm_count, instr_limit) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("FATAL: cannot load routes file '{routes_file}': {e}");
            std::process::exit(1);
        }
    };

    info!(
        "intermediate-proxy v2: {} proxies, {} routes, {} lua vms, instr_limit={}, probes≤{}, max_hard_attempts={}",
        pool.len(),
        lua.config().routes.len(),
        vm_count,
        instr_limit,
        max_concurrent_probes,
        max_hard_attempts,
    );

    tokio::spawn(proxy::watcher::run_proxy_watcher(
        Arc::clone(&pool),
        PathBuf::from(&proxy_file),
        Duration::from_millis(proxy_refresh_ms),
    ));
    tokio::spawn(lua::run_routes_watcher(
        Arc::clone(&lua),
        PathBuf::from(&routes_file),
        Duration::from_millis(routes_refresh_ms),
    ));
    tokio::spawn(route::prober::run_route_prober(
        Arc::clone(&pool),
        Arc::clone(&lua),
        max_concurrent_probes,
    ));
    tokio::spawn(route::prober::run_transport_prober(
        Arc::clone(&pool),
        transport_probe_url,
        Duration::from_millis(transport_probe_interval_ms),
        Duration::from_millis(transport_probe_timeout_ms),
        max_concurrent_probes,
    ));

    let state = Arc::new(AppState {
        pool: Arc::clone(&pool),
        lua: Arc::clone(&lua),
        max_hard_attempts,
    });

    let app = Router::new()
        .route("/health", get(health::health_handler))
        .route("/health/route/{name}", get(health::route_detail_handler))
        .route("/health/proxy", get(health::proxy_detail_handler))
        .route("/{*path}", any(handler::proxy_handler))
        .route("/", any(handler::proxy_handler))
        .layer(cors_layer())
        .with_state(state);

    if env_bool("TLS_ENABLED", false) {
        let domains_env = std::env::var("DOMAINS").unwrap_or_default();
        let domains = tls::parse_domains(&domains_env);
        if domains.is_empty() {
            panic!("TLS_ENABLED=true but DOMAINS is empty");
        }
        let email =
            std::env::var("ACME_EMAIL").unwrap_or_else(|_| format!("admin@{}", domains[0]));
        let cache_dir = PathBuf::from(
            std::env::var("ACME_CACHE_DIR").unwrap_or_else(|_| "/var/cache/acme".to_string()),
        );
        let staging = env_bool("ACME_STAGING", false);
        info!(
            "TLS mode on: {} domain(s), cache={:?}, staging={}",
            domains.len(),
            cache_dir,
            staging
        );
        tls::serve(domains, email, cache_dir, staging, app).await;
    } else {
        let port = port_from_env(3000);
        let listener = bind_tcp(port).await;
        info!("intermediate-proxy listening on http://0.0.0.0:{port}");
        axum::serve(listener, app).await.expect("server error");
    }
}
