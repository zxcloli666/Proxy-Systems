use url::Url;

/// Resolve a redirect Location header to an absolute URL.
/// Handles: absolute (`http://...`), protocol-relative (`//...`),
/// root-relative (`/path`), and relative (`path`) redirects.
pub fn resolve_redirect(location: &str, base_url: &str) -> Option<String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Some(location.to_string());
    }

    let base = Url::parse(base_url).ok()?;

    if location.starts_with("//") {
        return Some(format!("{}{}", base.scheme(), location));
    }

    // Both root-relative ("/path") and relative ("path") are handled by Url::join
    base.join(location).ok().map(|u| u.to_string())
}
