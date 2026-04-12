use axum::http::{HeaderValue, Method};
use tower_http::cors::{Any, CorsLayer};

pub fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers(Any)
        .max_age(std::time::Duration::from_secs(86400))
}

pub fn cors_headers() -> [(&'static str, HeaderValue); 3] {
    [
        (
            "access-control-allow-origin",
            HeaderValue::from_static("*"),
        ),
        (
            "access-control-allow-methods",
            HeaderValue::from_static("GET, POST, PUT, DELETE, OPTIONS"),
        ),
        (
            "access-control-allow-headers",
            HeaderValue::from_static("*"),
        ),
    ]
}
