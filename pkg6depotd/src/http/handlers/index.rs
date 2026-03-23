use crate::errors::DepotError;
use crate::repo::DepotRepo;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

pub async fn get_index_v0(
    State(repo): State<Arc<DepotRepo>>,
    Path((publisher, command)): Path<(String, String)>,
) -> Result<Response, DepotError> {
    match command.as_str() {
        "refresh" => {
            repo.rebuild_index(Some(&publisher))?;
            Ok(StatusCode::OK.into_response())
        }
        _ => Ok((StatusCode::BAD_REQUEST, "Unknown command").into_response()),
    }
}
