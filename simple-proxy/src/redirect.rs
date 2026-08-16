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
        return Some(format!("{}:{}", base.scheme(), location));
    }

    // Both root-relative ("/path") and relative ("path") are handled by Url::join
    base.join(location).ok().map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://www.cian.ru/rent/flat/326602837/";

    #[test]
    fn absolute_passes_through() {
        assert_eq!(
            resolve_redirect("https://nn.cian.ru/x/", BASE).as_deref(),
            Some("https://nn.cian.ru/x/")
        );
    }

    #[test]
    fn protocol_relative_keeps_the_colon() {
        assert_eq!(
            resolve_redirect("//nn.cian.ru/rent/flat/1/", BASE).as_deref(),
            Some("https://nn.cian.ru/rent/flat/1/")
        );
    }

    #[test]
    fn relative_resolves_against_base() {
        assert_eq!(
            resolve_redirect("/sale/flat/2/", BASE).as_deref(),
            Some("https://www.cian.ru/sale/flat/2/")
        );
    }
}
