use regex::Regex;

#[derive(Debug)]
pub enum HostMatch {
    Any,
    Exact(String),
    /// `*.example.com` → suffix match on `.example.com` (also matches the apex).
    Suffix(String),
    Regex(Regex),
}

#[derive(Debug)]
pub enum PathMatch {
    Any,
    Prefix(String),
    /// glob with `*` (any non-slash run) and `**` (any run).
    Glob(Regex),
    Regex(Regex),
}

#[derive(Debug)]
pub struct Matcher {
    pub host: HostMatch,
    pub path: PathMatch,
    pub methods: Option<Vec<String>>,
    pub has_predicate: bool,
}

impl HostMatch {
    pub fn matches(&self, host: &str) -> bool {
        match self {
            HostMatch::Any => true,
            HostMatch::Exact(h) => host.eq_ignore_ascii_case(h),
            HostMatch::Suffix(suffix) => {
                let apex = suffix.strip_prefix('.').unwrap_or(suffix);
                host.eq_ignore_ascii_case(apex)
                    || host.to_ascii_lowercase().ends_with(&suffix.to_ascii_lowercase())
            }
            HostMatch::Regex(re) => re.is_match(host),
        }
    }
}

impl PathMatch {
    pub fn matches(&self, path: &str) -> bool {
        match self {
            PathMatch::Any => true,
            PathMatch::Prefix(p) => path.starts_with(p.as_str()),
            PathMatch::Glob(re) | PathMatch::Regex(re) => re.is_match(path),
        }
    }
}

impl Matcher {
    /// Cheap pre-predicate check (host/path/method). The Lua predicate, if any,
    /// is evaluated separately by the caller only when this passes.
    pub fn matches_static(&self, host: &str, path: &str, method: &str) -> bool {
        if !self.host.matches(host) {
            return false;
        }
        if !self.path.matches(path) {
            return false;
        }
        if let Some(ms) = &self.methods {
            if !ms.iter().any(|m| m.eq_ignore_ascii_case(method)) {
                return false;
            }
        }
        true
    }
}

/// Translate a `*`/`**` glob into an anchored regex. `**` matches anything,
/// `*` matches any run that doesn't cross a `/`.
pub fn glob_to_regex(glob: &str) -> Result<Regex, regex::Error> {
    let mut re = String::with_capacity(glob.len() * 2 + 2);
    re.push('^');
    let bytes = glob.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'*' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    re.push_str(".*");
                    i += 2;
                } else {
                    re.push_str("[^/]*");
                    i += 1;
                }
            }
            b'?' => {
                re.push_str("[^/]");
                i += 1;
            }
            c => {
                let ch = c as char;
                if ".+()|[]{}^$\\".contains(ch) {
                    re.push('\\');
                }
                re.push(ch);
                i += 1;
            }
        }
    }
    re.push('$');
    Regex::new(&re)
}
