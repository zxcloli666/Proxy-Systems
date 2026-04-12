use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::cors::cors_headers;

/// Return a plain text error response with CORS headers.
pub fn text_response(status: StatusCode, body: &str) -> Response {
    let mut response = (status, body.to_string()).into_response();
    let headers = response.headers_mut();
    for (name, value) in cors_headers() {
        headers.insert(name, value);
    }
    headers.insert("content-type", HeaderValue::from_static("text/plain"));
    response
}

/// Return a JSON response with CORS headers.
pub fn json_response(status: StatusCode, body: &str) -> Response {
    let mut response = (status, body.to_string()).into_response();
    let headers = response.headers_mut();
    for (name, value) in cors_headers() {
        headers.insert(name, value);
    }
    headers.insert(
        "content-type",
        HeaderValue::from_static("application/json"),
    );
    response
}
