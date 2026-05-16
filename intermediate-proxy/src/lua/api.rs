use mlua::{Lua, Table, Value};

/// Request context handed to every hook (read-only on the Lua side).
pub struct HookCtx {
    pub route_name: String,
    pub target_url: String,
    pub host: String,
    pub path: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub attempt: u32,
    pub proxy_url: Option<String>,
    pub proxy_tags: Vec<String>,
    pub elapsed_ms: u64,
}

/// Response metadata handed to `on_response` / probe classifier.
pub struct HookRes {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body_preview: Option<String>,
    pub elapsed_ms: u64,
    pub bytes_received: u64,
}

#[derive(Debug, Clone)]
pub enum ResponseVerdict {
    Success,
    RetryOtherProxy { ban_for_ms: u64 },
    RetrySameProxy { delay_ms: u64 },
    ReturnAsIs,
    HardFail { reason: String },
    GiveUp { status: u16, body: String },
}

#[derive(Debug, Clone)]
pub enum ExhaustedVerdict {
    TryFatal,
    WaitProbe { timeout_ms: u64 },
    ReturnError { status: u16, body: String },
    RetryAll,
}

#[derive(Debug, Clone)]
pub enum ProbeOutcome {
    Ok,
    StillBanned,
    GiveUpRoute,
}

#[derive(Debug, Default, Clone)]
pub struct RequestMods {
    pub add_headers: Vec<(String, String)>,
    pub drop_headers: Vec<String>,
    pub override_all_tags: Option<Vec<String>>,
    pub force_proxy: Option<String>,
}

fn headers_table(lua: &Lua, headers: &[(String, String)]) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (k, v) in headers {
        t.set(k.as_str(), v.as_str())?;
    }
    Ok(t)
}

pub fn build_ctx_table(lua: &Lua, ctx: &HookCtx) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("route_name", ctx.route_name.as_str())?;
    t.set("target_url", ctx.target_url.as_str())?;
    t.set("host", ctx.host.as_str())?;
    t.set("path", ctx.path.as_str())?;
    t.set("method", ctx.method.as_str())?;
    t.set("headers", headers_table(lua, &ctx.headers)?)?;
    t.set("attempt", ctx.attempt)?;
    match &ctx.proxy_url {
        Some(u) => t.set("proxy_url", u.as_str())?,
        None => t.set("proxy_url", Value::Nil)?,
    }
    let tags = lua.create_table()?;
    for (i, tg) in ctx.proxy_tags.iter().enumerate() {
        tags.set(i + 1, tg.as_str())?;
    }
    t.set("proxy_tags", tags)?;
    t.set("elapsed_ms", ctx.elapsed_ms)?;
    Ok(t)
}

pub fn build_res_table(lua: &Lua, res: &HookRes) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("status", res.status)?;
    t.set("headers", headers_table(lua, &res.headers)?)?;
    match &res.body_preview {
        Some(b) => t.set("body_preview", b.as_str())?,
        None => t.set("body_preview", Value::Nil)?,
    }
    t.set("elapsed_ms", res.elapsed_ms)?;
    t.set("bytes_received", res.bytes_received)?;
    Ok(t)
}

fn vnum(t: &Table, key: &str) -> Option<f64> {
    match t.get::<Value>(key) {
        Ok(Value::Integer(i)) => Some(i as f64),
        Ok(Value::Number(n)) => Some(n),
        _ => None,
    }
}

fn vstr(t: &Table, key: &str) -> Option<String> {
    match t.get::<Value>(key) {
        Ok(Value::String(s)) => s.to_str().ok().map(|s| s.to_string()),
        _ => None,
    }
}

/// Parse a verdict table from `on_response`. Unknown / malformed → Success
/// (fail-open). `default_ban_ms` is used when a retry verdict omits it.
pub fn parse_response_verdict(v: &Value, default_ban_ms: u64) -> ResponseVerdict {
    let Value::Table(t) = v else {
        return ResponseVerdict::Success;
    };
    match vstr(t, "verdict").as_deref() {
        Some("success") => ResponseVerdict::Success,
        Some("retry_other_proxy") => ResponseVerdict::RetryOtherProxy {
            ban_for_ms: vnum(t, "ban_for_ms").map(|n| n as u64).unwrap_or(default_ban_ms),
        },
        Some("retry_same_proxy") => ResponseVerdict::RetrySameProxy {
            delay_ms: vnum(t, "delay_ms").map(|n| n as u64).unwrap_or(0),
        },
        Some("return_as_is") => ResponseVerdict::ReturnAsIs,
        Some("hard_fail") => ResponseVerdict::HardFail {
            reason: vstr(t, "reason").unwrap_or_else(|| "hard_fail".into()),
        },
        Some("give_up") => ResponseVerdict::GiveUp {
            status: vnum(t, "status").map(|n| n as u16).unwrap_or(502),
            body: vstr(t, "body").unwrap_or_else(|| "proxy gave up".into()),
        },
        _ => ResponseVerdict::Success,
    }
}

pub fn parse_exhausted_verdict(v: &Value) -> ExhaustedVerdict {
    let Value::Table(t) = v else {
        return ExhaustedVerdict::TryFatal;
    };
    match vstr(t, "verdict").as_deref() {
        Some("try_fatal") => ExhaustedVerdict::TryFatal,
        Some("wait_probe") => ExhaustedVerdict::WaitProbe {
            timeout_ms: vnum(t, "timeout_ms").map(|n| n as u64).unwrap_or(5000),
        },
        Some("return_error") => ExhaustedVerdict::ReturnError {
            status: vnum(t, "status").map(|n| n as u16).unwrap_or(502),
            body: vstr(t, "body").unwrap_or_else(|| "all proxies exhausted".into()),
        },
        Some("retry_all") => ExhaustedVerdict::RetryAll,
        _ => ExhaustedVerdict::TryFatal,
    }
}

pub fn parse_request_mods(v: &Value) -> RequestMods {
    let mut mods = RequestMods::default();
    let Value::Table(t) = v else {
        return mods;
    };
    if let Ok(Value::Table(add)) = t.get::<Value>("add_headers") {
        for pair in add.pairs::<String, String>().flatten() {
            mods.add_headers.push((pair.0.to_ascii_lowercase(), pair.1));
        }
    }
    if let Ok(Value::Table(drop)) = t.get::<Value>("drop_headers") {
        for h in drop.sequence_values::<String>().flatten() {
            mods.drop_headers.push(h.to_ascii_lowercase());
        }
    }
    if let Ok(Value::Table(tags)) = t.get::<Value>("override_pool") {
        if let Ok(Value::Table(list)) = tags.get::<Value>("tags") {
            mods.override_all_tags = Some(
                list.sequence_values::<String>().flatten().collect(),
            );
        }
    }
    if let Some(fp) = vstr(t, "force_proxy") {
        mods.force_proxy = Some(fp);
    }
    mods
}

pub fn parse_probe_outcome(v: &Value, ok_statuses: &[u16], status: u16) -> ProbeOutcome {
    match v {
        Value::String(s) => match s.to_str().as_deref() {
            Ok("ok") => ProbeOutcome::Ok,
            Ok("give_up_route") => ProbeOutcome::GiveUpRoute,
            _ => ProbeOutcome::StillBanned,
        },
        Value::Boolean(true) => ProbeOutcome::Ok,
        Value::Nil | Value::Boolean(false) => {
            if ok_statuses.contains(&status) {
                ProbeOutcome::Ok
            } else {
                ProbeOutcome::StillBanned
            }
        }
        _ => ProbeOutcome::StillBanned,
    }
}
