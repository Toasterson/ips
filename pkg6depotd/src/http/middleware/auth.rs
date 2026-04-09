use crate::config::Oauth2Config;
use axum::{
    extract::{FromRequestParts, Request},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{
    decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, TokenData, Validation,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Cached JWKS state for token validation.
///
/// Initialised once at startup from the configured OIDC provider's `jwks_uri`.
/// Keys are refreshed automatically when a token presents a `kid` that is not
/// in the cache — this handles key rotation without restarts.
#[derive(Clone)]
pub struct AuthState {
    jwks: Arc<RwLock<JwkSet>>,
    issuer: String,
    audience: String,
    required_scopes: Vec<String>,
    jwks_uri: String,
    http: reqwest::Client,
}

/// Claims we extract from a validated JWT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject (unique user identifier from the IdP)
    pub sub: String,
    /// Issuer
    pub iss: String,
    /// Audience (may be string or array — we accept both via serde)
    #[serde(default)]
    pub aud: Audience,
    /// Expiration (unix timestamp)
    pub exp: u64,
    /// Issued at (unix timestamp)
    #[serde(default)]
    pub iat: u64,
    /// Space-separated scopes (standard OIDC format)
    #[serde(default)]
    pub scope: String,
    /// Roles claim (custom, used for RBAC)
    #[serde(default)]
    pub roles: Vec<String>,
}

/// OIDC `aud` can be a single string or an array of strings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum Audience {
    #[default]
    None,
    Single(String),
    Multiple(Vec<String>),
}

impl Audience {
    pub fn contains(&self, expected: &str) -> bool {
        match self {
            Audience::None => false,
            Audience::Single(s) => s == expected,
            Audience::Multiple(v) => v.iter().any(|s| s == expected),
        }
    }
}

/// The authenticated user identity extracted from a valid JWT.
/// Injected into axum request extensions by the auth middleware.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    /// OIDC subject identifier
    pub subject: String,
    /// Scopes from the token
    pub scopes: Vec<String>,
    /// Roles from the token (custom claim for RBAC)
    pub roles: Vec<String>,
}

/// Errors from token validation.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing Authorization header")]
    MissingToken,
    #[error("invalid Authorization header format")]
    InvalidFormat,
    #[error("token validation failed: {0}")]
    ValidationFailed(#[from] jsonwebtoken::errors::Error),
    #[error("no matching key found for kid: {0}")]
    KeyNotFound(String),
    #[error("insufficient scopes: required {required}, have {actual}")]
    InsufficientScopes { required: String, actual: String },
    #[error("JWKS fetch failed: {0}")]
    JwksFetchError(String),
}

impl AuthState {
    /// Create a new AuthState from an Oauth2Config.
    ///
    /// Fetches the JWKS from the configured URI at startup.
    /// Returns `None` if OAuth2 is not configured.
    pub async fn from_config(config: &Option<Oauth2Config>) -> Option<Self> {
        let config = config.as_ref()?;
        let jwks_uri = config.jwks_uri.as_ref()?;
        let issuer = config.issuer.as_ref()?;

        let http = reqwest::Client::new();

        let jwks = match Self::fetch_jwks(&http, jwks_uri).await {
            Ok(jwks) => {
                info!(
                    "Loaded {} keys from JWKS endpoint {}",
                    jwks.keys.len(),
                    jwks_uri
                );
                jwks
            }
            Err(e) => {
                warn!(
                    "Failed to fetch JWKS from {} at startup: {}. Auth will retry on first request.",
                    jwks_uri, e
                );
                JwkSet { keys: vec![] }
            }
        };

        Some(AuthState {
            jwks: Arc::new(RwLock::new(jwks)),
            issuer: issuer.clone(),
            audience: config.audience.clone().unwrap_or_default(),
            required_scopes: config.required_scopes.clone().unwrap_or_default(),
            jwks_uri: jwks_uri.clone(),
            http,
        })
    }

    /// Fetch the JWKS document from the configured URI.
    async fn fetch_jwks(http: &reqwest::Client, uri: &str) -> Result<JwkSet, AuthError> {
        let resp = http
            .get(uri)
            .send()
            .await
            .map_err(|e| AuthError::JwksFetchError(e.to_string()))?;

        let jwks: JwkSet = resp
            .json()
            .await
            .map_err(|e| AuthError::JwksFetchError(e.to_string()))?;

        Ok(jwks)
    }

    /// Refresh the cached JWKS from the configured endpoint.
    async fn refresh_jwks(&self) -> Result<(), AuthError> {
        let new_jwks = Self::fetch_jwks(&self.http, &self.jwks_uri).await?;
        info!("Refreshed JWKS: {} keys loaded", new_jwks.keys.len());
        let mut cached = self.jwks.write().await;
        *cached = new_jwks;
        Ok(())
    }

    /// Validate a Bearer token and return the authenticated user.
    ///
    /// 1. Decode the JWT header to get the `kid`
    /// 2. Find the matching key in the cached JWKS
    /// 3. If not found, refresh JWKS and retry once (handles key rotation)
    /// 4. Validate signature, issuer, audience, expiration
    /// 5. Check required scopes
    /// 6. Return the AuthenticatedUser
    pub async fn validate_token(&self, token: &str) -> Result<AuthenticatedUser, AuthError> {
        let header = decode_header(token)?;
        let kid = header
            .kid
            .as_deref()
            .unwrap_or("");

        debug!("Validating token with kid={:?}, alg={:?}", kid, header.alg);

        // Try to find the key, refresh once if not found
        let token_data = match self.decode_with_cached_keys(token, kid, header.alg).await {
            Ok(td) => td,
            Err(AuthError::KeyNotFound(_)) => {
                debug!("Key not found in cache, refreshing JWKS");
                self.refresh_jwks().await?;
                self.decode_with_cached_keys(token, kid, header.alg).await?
            }
            Err(e) => return Err(e),
        };

        let claims = token_data.claims;

        // Check required scopes
        let token_scopes: Vec<String> = claims
            .scope
            .split_whitespace()
            .map(String::from)
            .collect();

        for required in &self.required_scopes {
            if !token_scopes.iter().any(|s| s == required) {
                return Err(AuthError::InsufficientScopes {
                    required: self.required_scopes.join(" "),
                    actual: claims.scope.clone(),
                });
            }
        }

        Ok(AuthenticatedUser {
            subject: claims.sub,
            scopes: token_scopes,
            roles: claims.roles,
        })
    }

    /// Attempt to decode the token using the cached JWKS.
    async fn decode_with_cached_keys(
        &self,
        token: &str,
        kid: &str,
        alg: Algorithm,
    ) -> Result<TokenData<Claims>, AuthError> {
        let jwks = self.jwks.read().await;

        // Find key by kid, or if no kid use first key matching the algorithm
        let jwk = if !kid.is_empty() {
            jwks.keys
                .iter()
                .find(|k| {
                    k.common.key_id.as_deref() == Some(kid)
                })
                .ok_or_else(|| AuthError::KeyNotFound(kid.to_string()))?
        } else {
            // No kid — try first key that matches the algorithm
            jwks.keys
                .first()
                .ok_or_else(|| AuthError::KeyNotFound("(no kid, no keys)".to_string()))?
        };

        let decoding_key = DecodingKey::from_jwk(jwk)?;

        let mut validation = Validation::new(alg);
        validation.set_issuer(&[&self.issuer]);
        if !self.audience.is_empty() {
            validation.set_audience(&[&self.audience]);
        } else {
            // Don't validate audience if not configured
            validation.validate_aud = false;
        }

        let token_data = decode::<Claims>(token, &decoding_key, &validation)?;

        Ok(token_data)
    }

    /// Extract a Bearer token from an Authorization header value.
    pub fn extract_bearer(auth_header: &str) -> Result<&str, AuthError> {
        auth_header
            .strip_prefix("Bearer ")
            .or_else(|| auth_header.strip_prefix("bearer "))
            .ok_or(AuthError::InvalidFormat)
    }
}

// -- Axum middleware --

impl AuthError {
    /// Map AuthError to an HTTP status code.
    fn status_code(&self) -> StatusCode {
        match self {
            AuthError::MissingToken | AuthError::InvalidFormat | AuthError::KeyNotFound(_) => {
                StatusCode::UNAUTHORIZED
            }
            AuthError::ValidationFailed(_) => StatusCode::UNAUTHORIZED,
            AuthError::InsufficientScopes { .. } => StatusCode::FORBIDDEN,
            AuthError::JwksFetchError(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = serde_json::json!({
            "error": self.to_string(),
        });
        (status, axum::Json(body)).into_response()
    }
}

/// Axum middleware that requires a valid Bearer token.
///
/// Usage in router:
/// ```ignore
/// use axum::middleware::from_fn_with_state;
///
/// let protected = Router::new()
///     .route("/publish", post(handler))
///     .layer(from_fn_with_state(auth_state.clone(), require_auth));
/// ```
pub async fn require_auth(
    axum::extract::State(auth): axum::extract::State<Arc<AuthState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, AuthError> {
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::MissingToken)?;

    let token = AuthState::extract_bearer(auth_header)?;
    let user = auth.validate_token(token).await?;

    debug!(
        "Authenticated user: sub={}, scopes={:?}, roles={:?}",
        user.subject, user.scopes, user.roles
    );

    // Inject the authenticated user into request extensions so handlers can access it
    request.extensions_mut().insert(user);

    Ok(next.run(request).await)
}

/// Axum extractor for `AuthenticatedUser`.
///
/// Use in handler signatures to get the authenticated user:
/// ```ignore
/// async fn my_handler(user: AuthenticatedUser) -> impl IntoResponse {
///     format!("Hello, {}", user.subject)
/// }
/// ```
impl<S: Send + Sync> FromRequestParts<S> for AuthenticatedUser {
    type Rejection = AuthError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthenticatedUser>()
            .cloned()
            .ok_or(AuthError::MissingToken)
    }
}
