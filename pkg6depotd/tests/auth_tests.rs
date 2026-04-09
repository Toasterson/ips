use axum::{
    Router,
    middleware::from_fn_with_state,
    response::IntoResponse,
    routing::get,
};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use pkg6depotd::http::middleware::{require_auth, AuthState, AuthenticatedUser};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tokio::net::TcpListener;

/// Claims matching the format AuthState expects.
#[derive(Serialize, Deserialize)]
struct TestClaims {
    sub: String,
    iss: String,
    aud: String,
    exp: u64,
    iat: u64,
    scope: String,
    roles: Vec<String>,
}

/// Create a test token with the given claims.
fn create_test_token(
    encoding_key: &EncodingKey,
    kid: &str,
    claims: &TestClaims,
) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_string());
    encode(&header, claims, encoding_key).unwrap()
}

/// Helper to get current unix timestamp.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Start a tiny JWKS HTTP server and return (url, join_handle).
async fn start_jwks_server(jwks_json: serde_json::Value) -> (String, tokio::task::JoinHandle<()>) {
    let jwks_body = serde_json::to_string(&jwks_json).unwrap();

    let app = Router::new().route(
        "/.well-known/jwks.json",
        get(move || {
            let body = jwks_body.clone();
            async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body,
                )
                    .into_response()
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (format!("http://{}", addr), handle)
}

/// Build AuthState from a running JWKS server.
async fn build_auth_state(
    jwks_url: &str,
    issuer: &str,
    audience: &str,
    required_scopes: Vec<String>,
) -> AuthState {
    let config = pkg6depotd::config::Oauth2Config {
        issuer: Some(issuer.to_string()),
        jwks_uri: Some(format!("{}/.well-known/jwks.json", jwks_url)),
        audience: Some(audience.to_string()),
        required_scopes: Some(required_scopes),
    };

    AuthState::from_config(&Some(config)).await.unwrap()
}

/// Build a JWKS JSON from a PEM public key.
/// This is hacky but avoids pulling in extra crates for tests.
fn pem_to_jwks_json(kid: &str) -> serde_json::Value {
    // We use jsonwebtoken's own types to build the JWK
    // But since we can't easily extract RSA components from PEM in pure Rust
    // without extra crates, we'll use a pre-computed JWK for our test key.
    let jwk_json = include_str!("fixtures/test_jwk.json");
    let mut jwk: serde_json::Value = serde_json::from_str(jwk_json).unwrap();
    jwk["kid"] = json!(kid);

    json!({
        "keys": [jwk]
    })
}

// -- Tests --

#[tokio::test]
async fn test_valid_token_passes() {
    let kid = "test-key-1";
    let issuer = "https://test-issuer.example.com";
    let audience = "pkg6depotd";

    let jwks_json = pem_to_jwks_json(kid);
    let (jwks_url, _handle) = start_jwks_server(jwks_json).await;

    let auth = build_auth_state(&jwks_url, issuer, audience, vec!["pkg:publish".into()]).await;

    let rsa_private_pem = include_str!("fixtures/test_rsa_private.pem");
    let encoding_key = EncodingKey::from_rsa_pem(rsa_private_pem.as_bytes()).unwrap();

    let claims = TestClaims {
        sub: "user-123".into(),
        iss: issuer.into(),
        aud: audience.into(),
        exp: now() + 3600,
        iat: now(),
        scope: "pkg:publish pkg:read".into(),
        roles: vec!["publisher".into()],
    };

    let token = create_test_token(&encoding_key, kid, &claims);
    let user = auth.validate_token(&token).await.unwrap();

    assert_eq!(user.subject, "user-123");
    assert!(user.scopes.contains(&"pkg:publish".to_string()));
    assert!(user.roles.contains(&"publisher".to_string()));
}

#[tokio::test]
async fn test_expired_token_rejected() {
    let kid = "test-key-1";
    let issuer = "https://test-issuer.example.com";
    let audience = "pkg6depotd";

    let jwks_json = pem_to_jwks_json(kid);
    let (jwks_url, _handle) = start_jwks_server(jwks_json).await;

    let auth = build_auth_state(&jwks_url, issuer, audience, vec![]).await;

    let rsa_private_pem = include_str!("fixtures/test_rsa_private.pem");
    let encoding_key = EncodingKey::from_rsa_pem(rsa_private_pem.as_bytes()).unwrap();

    let claims = TestClaims {
        sub: "user-123".into(),
        iss: issuer.into(),
        aud: audience.into(),
        exp: now() - 3600, // expired 1 hour ago
        iat: now() - 7200,
        scope: "pkg:publish".into(),
        roles: vec![],
    };

    let token = create_test_token(&encoding_key, kid, &claims);
    let result = auth.validate_token(&token).await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("ExpiredSignature") || err.contains("expired") || err.contains("validation"),
        "Expected expiration error, got: {}",
        err
    );
}

#[tokio::test]
async fn test_wrong_audience_rejected() {
    let kid = "test-key-1";
    let issuer = "https://test-issuer.example.com";

    let jwks_json = pem_to_jwks_json(kid);
    let (jwks_url, _handle) = start_jwks_server(jwks_json).await;

    let auth = build_auth_state(&jwks_url, issuer, "pkg6depotd", vec![]).await;

    let rsa_private_pem = include_str!("fixtures/test_rsa_private.pem");
    let encoding_key = EncodingKey::from_rsa_pem(rsa_private_pem.as_bytes()).unwrap();

    let claims = TestClaims {
        sub: "user-123".into(),
        iss: issuer.into(),
        aud: "wrong-audience".into(), // wrong audience
        exp: now() + 3600,
        iat: now(),
        scope: "".into(),
        roles: vec![],
    };

    let token = create_test_token(&encoding_key, kid, &claims);
    let result = auth.validate_token(&token).await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("InvalidAudience") || err.contains("audience") || err.contains("validation"),
        "Expected audience error, got: {}",
        err
    );
}

#[tokio::test]
async fn test_missing_scopes_rejected() {
    let kid = "test-key-1";
    let issuer = "https://test-issuer.example.com";
    let audience = "pkg6depotd";

    let jwks_json = pem_to_jwks_json(kid);
    let (jwks_url, _handle) = start_jwks_server(jwks_json).await;

    let auth =
        build_auth_state(&jwks_url, issuer, audience, vec!["pkg:publish".into()]).await;

    let rsa_private_pem = include_str!("fixtures/test_rsa_private.pem");
    let encoding_key = EncodingKey::from_rsa_pem(rsa_private_pem.as_bytes()).unwrap();

    let claims = TestClaims {
        sub: "user-123".into(),
        iss: issuer.into(),
        aud: audience.into(),
        exp: now() + 3600,
        iat: now(),
        scope: "pkg:read".into(), // missing pkg:publish
        roles: vec![],
    };

    let token = create_test_token(&encoding_key, kid, &claims);
    let result = auth.validate_token(&token).await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("insufficient scopes"),
        "Expected scope error, got: {}",
        err
    );
}

#[tokio::test]
async fn test_no_token_returns_401() {
    // Test the middleware path: no Authorization header → 401
    let kid = "test-key-1";
    let issuer = "https://test-issuer.example.com";
    let audience = "pkg6depotd";

    let jwks_json = pem_to_jwks_json(kid);
    let (jwks_url, _handle) = start_jwks_server(jwks_json).await;

    let auth =
        Arc::new(build_auth_state(&jwks_url, issuer, audience, vec![]).await);

    // Build a router with the auth middleware protecting a test route
    let app = Router::new()
        .route(
            "/protected",
            get(|user: AuthenticatedUser| async move {
                format!("Hello, {}", user.subject)
            }),
        )
        .layer(from_fn_with_state(auth.clone(), require_auth))
        .with_state(auth);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();

    // No token → 401
    let resp = client
        .get(format!("http://{}/protected", addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    // Invalid format → 401
    let resp = client
        .get(format!("http://{}/protected", addr))
        .header("Authorization", "Basic dXNlcjpwYXNz")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}
