# Phase 4: OpenID Connect Authentication

**Date:** 2026-02-25
**Status:** Active
**Depends on:** Phase 1 (architecture)
**Implements:** ADR-002

## Goal

Secure the REST API with OIDC JWT validation. Allow CLI and GUI clients to authenticate via standard OIDC flows.

## Step 1: Server-side JWT validation (pkg6depotd)

### 1.1: Add dependencies

```toml
# pkg6depotd/Cargo.toml
jsonwebtoken = "9"
reqwest = { version = "0.12", features = ["json"] }  # for JWKS fetch
serde_json = "1"
```

### 1.2: Configuration

```kdl
// depot.kdl
auth {
    enabled true
    oidc-issuer "https://keycloak.example.com/realms/ips"
    required-scopes "ips:read" "ips:write"
    // Optional: per-publisher access via JWT claims
    publisher-claim "ips_publishers"
}
```

### 1.3: JWKS fetcher

Background task that fetches and caches JWKS from the OIDC provider:

```rust
pub struct JwksCache {
    keys: RwLock<jwk::JwkSet>,
    issuer: String,
    jwks_uri: String,
}

impl JwksCache {
    pub async fn new(issuer: &str) -> Result<Self> { /* fetch .well-known/openid-configuration */ }
    pub async fn refresh(&self) -> Result<()> { /* re-fetch JWKS */ }
    pub fn validate_token(&self, token: &str) -> Result<Claims> { /* decode + verify */ }
}
```

### 1.4: Auth middleware

Axum middleware that validates Bearer tokens on protected routes:

```rust
pub async fn require_auth(
    State(jwks): State<Arc<JwksCache>>,
    req: Request,
    next: Next,
) -> Result<Response, DepotError> {
    let token = extract_bearer_token(&req)?;
    let claims = jwks.validate_token(&token)?;
    // Check required scopes
    // Inject claims into request extensions
    next.run(req).await
}
```

Apply to: POST routes (publish, index rebuild). Leave GET routes (catalog, manifest, file, search) unauthenticated by default. Add optional `auth.require-read true` config to protect everything.

## Step 2: Client-side OIDC (libips RestBackend)

### 2.1: Add dependencies

```toml
# libips/Cargo.toml
openidconnect = "4"
```

### 2.2: CredentialProvider trait

```rust
pub trait CredentialProvider: Send + Sync {
    fn get_token(&self) -> Result<String>;
    fn refresh_if_needed(&self) -> Result<()>;
}
```

### 2.3: Device Code Flow (CLI)

```rust
pub struct DeviceCodeProvider {
    issuer: String,
    client_id: String,
    token_path: PathBuf,  // cached token on disk
}
```

Flow:
1. Call device authorization endpoint
2. Print "Open https://... and enter code: ABCD-EFGH"
3. Poll token endpoint until user completes
4. Cache token + refresh token to `{image}/.pkg/auth/{publisher}.json`
5. On subsequent calls, use refresh token if access token expired

### 2.4: Wire into RestBackend

```rust
impl RestBackend {
    pub fn with_credentials(mut self, provider: Arc<dyn CredentialProvider>) -> Self {
        self.credential_provider = Some(provider);
        self
    }
}
```

All HTTP requests check for credential provider and add `Authorization: Bearer {token}` header.

## Step 3: Token storage

Tokens stored in image metadata:
```
{image_root}/.pkg/auth/
  {publisher}.json  -- { "access_token": "...", "refresh_token": "...", "expires_at": "..." }
```

File permissions: `0600` (owner read/write only).

## Verification

- Start Keycloak/Dex in Docker for testing
- Verify unauthenticated GET requests still work
- Verify protected POST requires valid Bearer token
- Verify expired tokens are rejected
- Verify CLI device code flow obtains and caches token
- Verify token refresh works transparently
