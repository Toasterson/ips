use axum::{
    Extension, Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use std::sync::Arc;

use crate::http::middleware::{AuthError, AuthState};
use crate::repo::DepotRepo;

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

pub async fn health(_state: State<Arc<DepotRepo>>) -> impl IntoResponse {
    (StatusCode::OK, Json(HealthResponse { status: "ok" }))
}

#[derive(Serialize)]
struct AuthCheckResponse {
    authenticated: bool,
    token_present: bool,
    subject: Option<String>,
    scopes: Vec<String>,
    roles: Vec<String>,
    decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Admin auth-check endpoint.
///
/// If OAuth2 is configured (AuthState extension present), performs real JWT
/// validation. Otherwise falls back to checking for Bearer token presence.
pub async fn auth_check(
    auth_state: Option<Extension<Arc<AuthState>>>,
    headers: HeaderMap,
) -> Response {
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let token_present = auth_header.is_some();

    // If no AuthState configured, just check token presence (dev mode)
    let Some(Extension(auth)) = auth_state else {
        let authenticated =
            matches!(auth_header, Some(h) if h.to_ascii_lowercase().starts_with("bearer "));
        let resp = AuthCheckResponse {
            authenticated,
            token_present,
            subject: None,
            scopes: vec![],
            roles: vec![],
            decision: if authenticated { "allow" } else { "deny" },
            error: if !authenticated && !token_present {
                Some("no Authorization header".to_string())
            } else if !authenticated {
                Some("OIDC not configured, cannot validate token".to_string())
            } else {
                None
            },
        };
        let status = if authenticated {
            StatusCode::OK
        } else {
            StatusCode::UNAUTHORIZED
        };
        return (status, Json(resp)).into_response();
    };

    // Real validation path
    let Some(header_val) = auth_header else {
        let resp = AuthCheckResponse {
            authenticated: false,
            token_present: false,
            subject: None,
            scopes: vec![],
            roles: vec![],
            decision: "deny",
            error: Some("missing Authorization header".to_string()),
        };
        return (StatusCode::UNAUTHORIZED, Json(resp)).into_response();
    };

    let token = match AuthState::extract_bearer(header_val) {
        Ok(t) => t,
        Err(e) => {
            let resp = AuthCheckResponse {
                authenticated: false,
                token_present: true,
                subject: None,
                scopes: vec![],
                roles: vec![],
                decision: "deny",
                error: Some(e.to_string()),
            };
            return (StatusCode::UNAUTHORIZED, Json(resp)).into_response();
        }
    };

    match auth.validate_token(token).await {
        Ok(user) => {
            let resp = AuthCheckResponse {
                authenticated: true,
                token_present: true,
                subject: Some(user.subject),
                scopes: user.scopes,
                roles: user.roles,
                decision: "allow",
                error: None,
            };
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err(e) => {
            let status = match &e {
                AuthError::InsufficientScopes { .. } => StatusCode::FORBIDDEN,
                _ => StatusCode::UNAUTHORIZED,
            };
            let resp = AuthCheckResponse {
                authenticated: false,
                token_present: true,
                subject: None,
                scopes: vec![],
                roles: vec![],
                decision: "deny",
                error: Some(e.to_string()),
            };
            (status, Json(resp)).into_response()
        }
    }
}
