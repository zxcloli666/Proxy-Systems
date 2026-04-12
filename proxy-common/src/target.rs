use base64::Engine;

/// Decode a base64-encoded URL from the X-Target header.
/// Returns `None` if the header is missing, invalid base64, or not a valid URL.
pub fn decode_target(header_value: &str) -> Option<String> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(header_value)
        .ok()?;
    let url = String::from_utf8(decoded).ok()?;
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(url)
    } else {
        None
    }
}
