use axum::extract::State;
use axum::response::Response;
use proxy_common::response::json_response;

use crate::handler::HandlerState;

pub async fn health_handler(State(state): State<HandlerState>) -> Response {
    let nodes = state.pool.status().await;
    let body = serde_json::json!({
        "status": "ok",
        "nodes": nodes,
    });
    json_response(axum::http::StatusCode::OK, &body.to_string())
}
