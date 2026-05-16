pub mod api;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use mlua::{HookTriggers, Lua, Table, Value};
use parking_lot::Mutex;
use tracing::{error, warn};

pub use api::{
    ExhaustedVerdict, HookCtx, HookRes, ProbeOutcome, RequestMods, ResponseVerdict,
};

use crate::proxy::RouteId;
use crate::route::config::{parse_router_config, RouterConfig};

/// A pool of independent Lua VMs. Each VM loads the same `routes.lua` source
/// and keeps the route hooks flattened under the `__HOOKS` registry table,
/// addressed by 1-based route id. Calls are CPU-only and never `.await`, so a
/// per-VM mutex held only for the duration of a synchronous call is fine.
pub struct LuaPool {
    vms: ArcSwap<Vec<Arc<Mutex<Lua>>>>,
    rr: AtomicUsize,
    config: ArcSwap<RouterConfig>,
    instr_limit: u32,
    routes_dir: String,
}

fn sandbox(lua: &Lua, routes_dir: &str) -> mlua::Result<()> {
    let g = lua.globals();
    for danger in ["io", "dofile", "loadfile", "load", "loadstring"] {
        g.set(danger, Value::Nil)?;
    }
    if let Ok(Value::Table(os)) = g.get::<Value>("os") {
        for danger in [
            "execute", "exit", "remove", "rename", "tmpname", "setlocale", "getenv",
        ] {
            os.set(danger, Value::Nil)?;
        }
    }
    if let Ok(Value::Table(pkg)) = g.get::<Value>("package") {
        pkg.set("cpath", "")?;
        pkg.set(
            "path",
            format!("{routes_dir}/?.lua;{routes_dir}/?/init.lua"),
        )?;
    }

    let log = lua.create_table()?;
    macro_rules! log_fn {
        ($name:literal, $lvl:ident) => {{
            let f = lua.create_function(|_, msg: String| {
                tracing::$lvl!(target: "lua_route", "{}", msg);
                Ok(())
            })?;
            log.set($name, f)?;
        }};
    }
    log_fn!("info", info);
    log_fn!("warn", warn);
    log_fn!("debug", debug);
    log_fn!("error", error);
    g.set("log", log)?;
    Ok(())
}

fn build_hooks(lua: &Lua, cfg: &Table) -> mlua::Result<()> {
    let hooks = lua.create_table()?;
    if let Ok(Value::Table(routes)) = cfg.get::<Value>("routes") {
        let mut i = 0i64;
        for rv in routes.sequence_values::<Table>() {
            let r = rv?;
            i += 1;
            let e = lua.create_table()?;
            for name in ["on_request", "on_response", "on_exhausted"] {
                if let Ok(Value::Function(f)) = r.get::<Value>(name) {
                    e.set(name, f)?;
                }
            }
            if let Ok(Value::Table(m)) = r.get::<Value>("match") {
                if let Ok(Value::Function(f)) = m.get::<Value>("predicate") {
                    e.set("predicate", f)?;
                }
            }
            if let Ok(Value::Table(p)) = r.get::<Value>("probe") {
                if let Ok(Value::Function(f)) = p.get::<Value>("classify") {
                    e.set("probe_classify", f)?;
                }
            }
            hooks.set(i, e)?;
        }
    }
    lua.set_named_registry_value("__HOOKS", hooks)?;
    Ok(())
}

fn new_vm(source: &str, routes_dir: &str, instr_limit: u32) -> Result<(Lua, Table), String> {
    let lua = Lua::new();
    sandbox(&lua, routes_dir).map_err(|e| format!("sandbox: {e}"))?;

    let cfg: Table = lua
        .load(source)
        .set_name("@routes.lua")
        .call(())
        .map_err(|e| format!("load routes.lua: {e}"))?;

    build_hooks(&lua, &cfg).map_err(|e| format!("flatten hooks: {e}"))?;

    if instr_limit > 0 {
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(instr_limit),
            |_, _| {
                Err(mlua::Error::RuntimeError(
                    "lua instruction budget exceeded".into(),
                ))
            },
        );
    }
    Ok((lua, cfg))
}

impl LuaPool {
    pub fn build(
        source: &str,
        routes_path: &str,
        vm_count: usize,
        instr_limit: u32,
    ) -> Result<Arc<Self>, String> {
        let routes_dir = Path::new(routes_path)
            .parent()
            .and_then(|p| p.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(".")
            .to_string();

        let n = vm_count.max(1);
        let mut vms = Vec::with_capacity(n);
        let mut config: Option<RouterConfig> = None;
        for k in 0..n {
            let (lua, cfg) = new_vm(source, &routes_dir, instr_limit)?;
            if k == 0 {
                config = Some(parse_router_config(&lua, &cfg)?);
            }
            vms.push(Arc::new(Mutex::new(lua)));
        }
        let config = config.ok_or("no vm built")?;
        Ok(Arc::new(Self {
            vms: ArcSwap::from_pointee(vms),
            rr: AtomicUsize::new(0),
            config: ArcSwap::from_pointee(config),
            instr_limit,
            routes_dir,
        }))
    }

    pub fn reload(&self, source: &str) -> Result<(), String> {
        let n = self.vms.load().len().max(1);
        let mut vms = Vec::with_capacity(n);
        let mut config: Option<RouterConfig> = None;
        for k in 0..n {
            let (lua, cfg) = new_vm(source, &self.routes_dir, self.instr_limit)?;
            if k == 0 {
                config = Some(parse_router_config(&lua, &cfg)?);
            }
            vms.push(Arc::new(Mutex::new(lua)));
        }
        self.config.store(Arc::new(config.ok_or("no vm built")?));
        self.vms.store(Arc::new(vms));
        Ok(())
    }

    #[inline]
    pub fn config(&self) -> Arc<RouterConfig> {
        self.config.load_full()
    }

    fn pick(&self) -> Arc<Mutex<Lua>> {
        let vms = self.vms.load();
        let n = vms.len();
        let start = self.rr.fetch_add(1, Ordering::Relaxed) % n;
        for off in 0..n {
            let idx = (start + off) % n;
            if vms[idx].try_lock().is_some() {
                return Arc::clone(&vms[idx]);
            }
        }
        Arc::clone(&vms[start])
    }

    fn hook_fn(lua: &Lua, route_id: RouteId, name: &str) -> Option<mlua::Function> {
        let hooks: Table = lua.named_registry_value("__HOOKS").ok()?;
        let entry: Table = hooks.get(route_id as i64).ok()?;
        match entry.get::<Value>(name) {
            Ok(Value::Function(f)) => Some(f),
            _ => None,
        }
    }

    pub fn eval_predicate(&self, route_id: RouteId, ctx: &HookCtx) -> bool {
        let vm = self.pick();
        let lua = vm.lock();
        let Some(f) = Self::hook_fn(&lua, route_id, "predicate") else {
            return true;
        };
        let Ok(req) = api::build_ctx_table(&lua, ctx) else {
            return true;
        };
        match f.call::<Value>(req) {
            Ok(Value::Boolean(b)) => b,
            Ok(Value::Nil) => false,
            Ok(_) => true,
            Err(e) => {
                warn!(target: "lua_route", "predicate '{}' error: {}", ctx.route_name, e);
                false
            }
        }
    }

    pub fn on_request(&self, route_id: RouteId, ctx: &HookCtx) -> RequestMods {
        let vm = self.pick();
        let lua = vm.lock();
        let Some(f) = Self::hook_fn(&lua, route_id, "on_request") else {
            return RequestMods::default();
        };
        let Ok(c) = api::build_ctx_table(&lua, ctx) else {
            return RequestMods::default();
        };
        match f.call::<Value>(c) {
            Ok(v) => api::parse_request_mods(&v),
            Err(e) => {
                warn!(target: "lua_route", "on_request '{}' error: {}", ctx.route_name, e);
                RequestMods::default()
            }
        }
    }

    pub fn on_response(
        &self,
        route_id: RouteId,
        ctx: &HookCtx,
        res: &HookRes,
        default_ban_ms: u64,
    ) -> ResponseVerdict {
        let vm = self.pick();
        let lua = vm.lock();
        let Some(f) = Self::hook_fn(&lua, route_id, "on_response") else {
            return ResponseVerdict::Success;
        };
        let (Ok(c), Ok(r)) = (
            api::build_ctx_table(&lua, ctx),
            api::build_res_table(&lua, res),
        ) else {
            return ResponseVerdict::Success;
        };
        match f.call::<Value>((c, r)) {
            Ok(v) => api::parse_response_verdict(&v, default_ban_ms),
            Err(e) => {
                warn!(target: "lua_route", "on_response '{}' error: {}", ctx.route_name, e);
                ResponseVerdict::Success
            }
        }
    }

    pub fn on_exhausted(&self, route_id: RouteId, ctx: &HookCtx) -> ExhaustedVerdict {
        let vm = self.pick();
        let lua = vm.lock();
        let Some(f) = Self::hook_fn(&lua, route_id, "on_exhausted") else {
            return ExhaustedVerdict::TryFatal;
        };
        let Ok(c) = api::build_ctx_table(&lua, ctx) else {
            return ExhaustedVerdict::TryFatal;
        };
        match f.call::<Value>(c) {
            Ok(v) => api::parse_exhausted_verdict(&v),
            Err(e) => {
                warn!(target: "lua_route", "on_exhausted '{}' error: {}", ctx.route_name, e);
                ExhaustedVerdict::TryFatal
            }
        }
    }

    pub fn probe_classify(
        &self,
        route_id: RouteId,
        status: u16,
        headers: &[(String, String)],
        body_preview: Option<&str>,
        ok_statuses: &[u16],
    ) -> ProbeOutcome {
        let vm = self.pick();
        let lua = vm.lock();
        let Some(f) = Self::hook_fn(&lua, route_id, "probe_classify") else {
            return if ok_statuses.contains(&status) {
                ProbeOutcome::Ok
            } else {
                ProbeOutcome::StillBanned
            };
        };
        let h = match lua.create_table() {
            Ok(t) => {
                for (k, v) in headers {
                    let _ = t.set(k.as_str(), v.as_str());
                }
                t
            }
            Err(_) => return ProbeOutcome::StillBanned,
        };
        let body = body_preview.unwrap_or("");
        match f.call::<Value>((status, h, body)) {
            Ok(v) => api::parse_probe_outcome(&v, ok_statuses, status),
            Err(e) => {
                error!(target: "lua_route", "probe classify error: {}", e);
                ProbeOutcome::StillBanned
            }
        }
    }
}

/// Load + compile at startup. On failure we cannot serve sensibly, so the
/// caller decides whether to exit.
pub fn build_from_file(
    path: &str,
    vm_count: usize,
    instr_limit: u32,
) -> Result<Arc<LuaPool>, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read routes file {path}: {e}"))?;
    LuaPool::build(&source, path, vm_count, instr_limit)
}

impl std::fmt::Debug for LuaPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LuaPool({} vms)", self.vms.load().len())
    }
}

/// Poll the routes file mtime and hot-reload Lua on change. A failed parse is
/// logged and the current config keeps serving — never a half-applied state.
pub async fn run_routes_watcher(
    pool: Arc<LuaPool>,
    path: std::path::PathBuf,
    interval: std::time::Duration,
) {
    let mut last = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
    loop {
        tokio::time::sleep(interval).await;
        let mtime = match tokio::fs::metadata(&path).await {
            Ok(m) => m.modified().ok(),
            Err(e) => {
                warn!("routes watcher: cannot stat {:?}: {}", path, e);
                continue;
            }
        };
        if mtime == last {
            continue;
        }
        last = mtime;
        match tokio::fs::read_to_string(&path).await {
            Ok(src) => match pool.reload(&src) {
                Ok(()) => tracing::info!(
                    "routes reloaded: {} route(s)",
                    pool.config().routes.len()
                ),
                Err(e) => error!("routes reload failed, keeping current config: {}", e),
            },
            Err(e) => warn!("routes watcher: cannot read {:?}: {}", path, e),
        }
    }
}
