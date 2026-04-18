use axum::extract::State;
use axum::response::Response;
use proxy_common::response::json_response;
use std::sync::Arc;

use crate::queue::ProxyQueue;

pub async fn health_handler(State(queue): State<Arc<ProxyQueue>>) -> Response {
    let status = queue.status();
    json_response(axum::http::StatusCode::OK, &status.to_string())
}
