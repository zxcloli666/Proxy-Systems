use std::time::{SystemTime, UNIX_EPOCH};

#[inline]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Mask the password portion of `scheme://user:pass@host/...`.
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

/// Scrub `user:pass@` credentials from URL-looking substrings in free text.
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

pub fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

pub fn env_string(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

pub fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}
