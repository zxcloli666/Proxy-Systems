pub mod config;
pub mod matcher;
pub mod prober;
pub mod state;

use std::sync::Arc;

use crate::lua::{HookCtx, LuaPool};
use config::{RouteDef, RouterConfig};

/// First route whose static matcher accepts the request and whose Lua
/// predicate (if any) returns truthy. Routes are tried in file order.
pub fn select_route(
    lua: &LuaPool,
    cfg: &RouterConfig,
    host: &str,
    path: &str,
    method: &str,
    ctx: &HookCtx,
) -> Option<Arc<RouteDef>> {
    for r in cfg.candidates(host, path, method) {
        if r.matcher.has_predicate && !lua.eval_predicate(r.id, ctx) {
            continue;
        }
        return Some(Arc::clone(r));
    }
    None
}
