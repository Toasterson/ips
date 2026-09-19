use crate::errors::DepotError;
use crate::repo::DepotRepo;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use libips::repository::IndexEntry;
use std::sync::Arc;

pub async fn get_search_v0(
    State(repo): State<Arc<DepotRepo>>,
    Path((publisher, token)): Path<(String, String)>,
) -> Result<Response, DepotError> {
    search_v0_impl(&repo, Some(&publisher), &token)
}

pub async fn get_default_search_v0(
    State(repo): State<Arc<DepotRepo>>,
    Path(token): Path<String>,
) -> Result<Response, DepotError> {
    search_v0_impl(&repo, None, &token)
}

fn search_v0_impl(
    repo: &DepotRepo,
    publisher: Option<&str>,
    token: &str,
) -> Result<Response, DepotError> {
    let results = repo.search(publisher, token, false)?;

    // No results -> 204 No Content per pkg5 spec
    if results.is_empty() {
        return Ok(axum::http::StatusCode::NO_CONTENT.into_response());
    }

    // Format: {index_type} {fmri} {action_type} {value}
    let mut body = String::new();
    for entry in results {
        body.push_str(&format!(
            "{} {} {} {}\n",
            entry.index_type, entry.fmri, entry.action_type, entry.value
        ));
    }

    Ok(([(axum::http::header::CONTENT_TYPE, "text/plain")], body).into_response())
}

pub async fn get_search_v1(
    State(repo): State<Arc<DepotRepo>>,
    Path((publisher, token)): Path<(String, String)>,
) -> Result<Response, DepotError> {
    search_v1_impl(&repo, Some(&publisher), &token)
}

pub async fn get_default_search_v1(
    State(repo): State<Arc<DepotRepo>>,
    Path(token): Path<String>,
) -> Result<Response, DepotError> {
    search_v1_impl(&repo, None, &token)
}

pub async fn post_search_v1(
    State(repo): State<Arc<DepotRepo>>,
    Path(publisher): Path<String>,
    body: String,
) -> Result<Response, DepotError> {
    // The POST body contains the token directly
    let token = body.trim();
    search_v1_impl(&repo, Some(&publisher), token)
}

pub async fn post_default_search_v1(
    State(repo): State<Arc<DepotRepo>>,
    body: String,
) -> Result<Response, DepotError> {
    let token = body.trim();
    search_v1_impl(&repo, None, token)
}

fn search_v1_impl(
    repo: &DepotRepo,
    publisher: Option<&str>,
    token: &str,
) -> Result<Response, DepotError> {
    // Search v1 token format: "<case>_<rtype>_<trans>_<installroot>_<query>"
    // Example: "False_2_None_None_%3A%3A%3Apostgres" -> query ":::postgres"
    let (prefix, query) = if let Some((p, q)) = split_v1_token(token) {
        (p, q)
    } else {
        ("False_2_None_None", token)
    };

    // Parse prefix fields
    let parts: Vec<&str> = prefix.split('_').collect();
    let case_sensitive = parts.first().map(|s| *s == "True").unwrap_or(false);
    let return_type = parts.get(1).copied().unwrap_or("2");

    // Run search
    let results = repo.search(publisher, query, case_sensitive)?;

    // No results -> 204 No Content per v1 spec
    if results.is_empty() {
        return Ok((axum::http::StatusCode::NO_CONTENT).into_response());
    }

    let body = format_v1_results(&results, return_type);
    Ok(([(axum::http::header::CONTENT_TYPE, "text/plain")], body).into_response())
}

/// Format v1 search results.
///
/// return_type "1" -> package-only: `{query_number} {return_type} {fmri}`
/// return_type "2" (default) -> actions: `{query_number} {return_type} {fmri} {matched_value_urlencoded} {action_line}`
fn format_v1_results(results: &[IndexEntry], return_type: &str) -> String {
    let mut body = String::from("Return from search v1\n");
    let query_number = 0;

    for entry in results {
        match return_type {
            "1" => {
                body.push_str(&format!(
                    "{} {} {}\n",
                    query_number, return_type, entry.fmri
                ));
            }
            _ => {
                let matched_value = urlencoding::encode(&entry.value);
                let mut action_line = entry.action_type.clone();
                for (k, v) in &entry.attributes {
                    action_line.push_str(&format!(" {}={}", k, v));
                }
                body.push_str(&format!(
                    "{} {} {} {} {}\n",
                    query_number, return_type, entry.fmri, matched_value, action_line
                ));
            }
        }
    }

    body
}

fn split_v1_token(token: &str) -> Option<(&str, &str)> {
    // Find the 4th underscore to split prefix from query
    let mut underscore_count = 0;
    for (i, c) in token.char_indices() {
        if c == '_' {
            underscore_count += 1;
            if underscore_count == 4 {
                return Some((&token[..i], &token[i + 1..]));
            }
        }
    }
    None
}
