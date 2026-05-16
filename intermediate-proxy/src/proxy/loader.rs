#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedProxy {
    pub url: String,
    pub tags: Vec<String>,
}

/// Parse a proxies file. One proxy per line:
///
///   scheme://host:port  tags=a,b,c
///   scheme://host:port  bare-tag another-tag
///
/// Blank lines and `#` comments are ignored. The first whitespace-delimited
/// token is the URL; remaining tokens are tags (`tags=` prefix optional).
/// Duplicate URLs keep the first occurrence (tags merged).
pub fn parse_proxies(content: &str) -> Vec<ParsedProxy> {
    let mut out: Vec<ParsedProxy> = Vec::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(url) = it.next() else {
            continue;
        };
        if !url.contains("://") {
            continue;
        }
        let mut tags: Vec<String> = Vec::new();
        for tok in it {
            if let Some(rest) = tok.strip_prefix("tags=") {
                for t in rest.split(',') {
                    let t = t.trim();
                    if !t.is_empty() && !tags.iter().any(|x| x == t) {
                        tags.push(t.to_string());
                    }
                }
            } else if !tok.starts_with('#') && !tags.iter().any(|x| x == tok) {
                tags.push(tok.to_string());
            } else if tok.starts_with('#') {
                break;
            }
        }
        if let Some(existing) = out.iter_mut().find(|p| p.url == url) {
            for t in tags {
                if !existing.tags.contains(&t) {
                    existing.tags.push(t);
                }
            }
        } else {
            out.push(ParsedProxy {
                url: url.to_string(),
                tags,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic() {
        let s = "\
# comment
http://localhost:8080
socks5://x.example.com:1080  tags=anon,us
forward://y.example.com:3128 datacenter eu

socks5://z.example.com:1080 tags=reserve  # trailing comment
";
        let p = parse_proxies(s);
        assert_eq!(p.len(), 4);
        assert_eq!(p[0].url, "http://localhost:8080");
        assert!(p[0].tags.is_empty());
        assert_eq!(p[1].tags, vec!["anon", "us"]);
        assert_eq!(p[2].tags, vec!["datacenter", "eu"]);
        assert_eq!(p[3].tags, vec!["reserve"]);
    }

    #[test]
    fn dedups_url_merging_tags() {
        let s = "socks5://a:1\nsocks5://a:1 tags=anon\n";
        let p = parse_proxies(s);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].tags, vec!["anon"]);
    }

    #[test]
    fn skips_garbage() {
        let s = "not-a-url\n   \nhttp://ok:1\n";
        let p = parse_proxies(s);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].url, "http://ok:1");
    }
}
