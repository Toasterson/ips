use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Serialize;
use std::sync::Arc;

use crate::repo::DepotRepo;

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

pub async fn health(_state: State<Arc<DepotRepo>>) -> impl IntoResponse {
    (StatusCode::OK, Json(HealthResponse { status: "ok" }))
}
