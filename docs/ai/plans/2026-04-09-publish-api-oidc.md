# Publish API with OIDC Authentication

**Date:** 2026-04-09  
**Status:** Active  
**Issue:** [#3 Server Publish](https://code.aopc.cloud/OpenIndiana/ips/issues/3)

## Goal

A transactional REST API for publishing packages to a pkg6 repository, protected by OIDC Bearer token authentication. Clients upload packages as `.p6p` or `.p5p` archives, or via individual manifest+file transactions. No pkg5 `pkgsend` wire compatibility — we define our own clean protocol.

## Decisions

- **No pkg5 client compatibility** for publish. We accept `.p5p` archives (legacy format) and `.p6p` archives (our format), plus individual manifest+file transactions.
- **No transaction timeout.** Transactions persist until explicitly committed or abandoned.
- **RBAC model** for publisher-level access control. JWT claims map to roles per publisher.

## Publication Protocol

### Endpoints

| Method | Path | Auth | Purpose |
|--------|------|------|---------|
| POST | `/{publisher}/open/0/{fmri}` | Required | Start a transaction, returns `Transaction-ID` |
| POST | `/{publisher}/add/0/{txn-id}` | Required | Add manifest or file payload to transaction |
| POST | `/{publisher}/close/0/{txn-id}` | Required | Commit transaction → package is published |
| POST | `/{publisher}/abandon/0/{txn-id}` | Required | Abort transaction, discard all data |
| GET | `/{publisher}/status/0/{txn-id}` | Required | Query transaction state (what files are still needed) |
| POST | `/{publisher}/publish/0/archive` | Required | Upload a .p6p or .p5p archive (batch publish) |

### Transaction Lifecycle

```
Client                          Depot
  │                               │
  ├─ POST /open/0/{fmri} ───────►│  Create Transaction
  │◄── 200 { txn_id, needed: [] }│  Return txn ID
  │                               │
  ├─ POST /add/0/{txn-id} ──────►│  Add manifest (multipart)
  │   Content-Type: text/plain    │  Parse manifest, compute needed files
  │◄── 200 { needed: [hash1,…] } │  Return list of missing payloads
  │                               │
  ├─ POST /add/0/{txn-id} ──────►│  Upload file payload
  │   X-IPkg-File-Hash: {hash}   │  Store in transaction staging area
  │◄── 200 { needed: [hash2,…] } │  Return remaining needed files
  │                               │
  ├─ POST /close/0/{txn-id} ────►│  Commit: move to repo atomically
  │◄── 200 { fmri, published }   │  Rebuild catalog, return published FMRI
  │                               │
  (or)                            │
  ├─ POST /abandon/0/{txn-id} ──►│  Discard transaction data
  │◄── 200                        │
```

### Transaction State

```rust
pub struct PublishTransaction {
    id: String,
    publisher: String,
    fmri: Fmri,
    state: TransactionState,   // Open, Submitted, Published, Abandoned
    manifest: Option<Manifest>,
    received_files: HashSet<String>,  // hashes of received payloads
    needed_files: Vec<String>,        // hashes still missing
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    subject: String,           // OIDC subject (who opened this)
    staging_dir: PathBuf,      // temp dir for this transaction's files
}
```

Transactions are stored in `{repo}/trans/{txn-id}/` on disk. On crash recovery, incomplete transactions can be resumed via the status endpoint. No automatic timeout — transactions persist until the client commits or abandons.

## OIDC Authentication

### Approach

Use **`jsonwebtoken`** (v10) + **`reqwest`** for JWKS fetching with a cached key set. This gives us full control over validation and doesn't pull in a heavy framework.

### Config

Extend the existing `Oauth2Config`:

```kdl
oauth2 {
    issuer "https://id.example.com"
    jwks-uri "https://id.example.com/.well-known/jwks.json"
    audience "pkg6depotd"
    required-scopes "pkg:publish"
}
```

### Middleware Design

```rust
// New file: pkg6depotd/src/http/middleware/auth.rs

pub struct AuthState {
    jwks: Arc<RwLock<JwkSet>>,       // cached JWKS
    issuer: String,
    audience: String,
    required_scopes: Vec<String>,
    jwks_uri: String,
}

/// Extracted from a validated JWT and injected into request extensions.
pub struct AuthenticatedUser {
    pub subject: String,             // OIDC `sub` claim
    pub scopes: Vec<String>,         // from `scope` claim
    pub roles: Vec<String>,          // from custom `roles` claim
}
```

The middleware:
1. Extracts `Authorization: Bearer {token}` header
2. Decodes JWT header to get `kid` (key ID)
3. Looks up signing key in cached JWKS (refresh if `kid` not found)
4. Validates signature, `iss`, `aud`, `exp`, `nbf`
5. Checks `scope` claim against `required_scopes`
6. Extracts `sub` and `roles` claims
7. Injects `AuthenticatedUser` into request extensions

### Route Protection

```rust
// In routes.rs — publish routes get the auth layer
let publish_routes = Router::new()
    .route("/{publisher}/open/0/{fmri}", post(publish::open))
    .route("/{publisher}/add/0/{txn_id}", post(publish::add))
    .route("/{publisher}/close/0/{txn_id}", post(publish::close))
    .route("/{publisher}/abandon/0/{txn_id}", post(publish::abandon))
    .route("/{publisher}/status/0/{txn_id}", get(publish::status))
    .route("/{publisher}/publish/0/archive", post(publish::upload_archive))
    .layer(from_fn_with_state(auth_state, require_auth));

// Read routes remain unprotected
let read_routes = Router::new()
    .route("/{publisher}/manifest/0/{fmri}", get(manifest::get_manifest))
    // ... etc
```

Only registered when repository mode is not `readonly`.

## RBAC Model

Role-based access control per publisher. Roles are stored in the repository and mapped from OIDC subjects.

### Roles

| Role | Permissions |
|------|-------------|
| `admin` | All operations on a publisher (publish, delete, manage roles) |
| `publisher` | Open transactions, upload packages, commit |
| `reader` | Read-only (default for unauthenticated, no special handling needed) |

### Storage

```rust
// In libips repository config or a dedicated SQLite DB
pub struct PublisherRole {
    pub publisher: String,
    pub subject: String,        // OIDC subject identifier
    pub role: Role,             // admin, publisher
    pub granted_by: String,     // who granted this role
    pub granted_at: DateTime<Utc>,
}
```

Stored in `{repo}/publisher/{name}/rbac.db` (SQLite). The auth middleware checks the role after JWT validation:

1. Validate JWT → get `AuthenticatedUser.subject`
2. Look up `subject` in `rbac.db` for the target `publisher`
3. Check if the role permits the requested operation
4. Reject with 403 if insufficient permissions

### Management

```
POST /admin/rbac/{publisher}/grant   — grant a role to a subject
DELETE /admin/rbac/{publisher}/revoke — revoke a role
GET /admin/rbac/{publisher}/list      — list role assignments
```

These admin endpoints require the `admin` role on the target publisher (or a global admin scope).

## Implementation Phases

### Phase 1: OIDC Token Validation
1. Add `jsonwebtoken` to pkg6depotd deps
2. Implement JWKS fetcher with caching (`AuthState`)
3. Implement `require_auth` axum middleware
4. Wire up to existing `auth_check` endpoint (replace placeholder)
5. Add `audience` field to `Oauth2Config`
6. Test with a real OIDC provider

### Phase 2: Publish Transaction API
1. Implement `PublishTransaction` state machine in libips
2. Add `open/0` handler — creates transaction, returns ID
3. Add `add/0` handler — accepts manifest or file payload
4. Add `close/0` handler — commits via `FileBackend::Transaction`
5. Add `abandon/0` and `status/0` handlers
6. Only register publish routes when mode != readonly

### Phase 3: Archive Upload
1. Add `publish/0/archive` handler — accepts .p6p or .p5p upload
2. Detect format by extension/magic bytes
3. For .p5p: use existing `Pkg5Importer` pipeline
4. For .p6p: use `ArchiveBackend` + receiver pipeline
5. Returns summary of published packages

### Phase 4: RBAC
1. Create `rbac.db` schema and access layer in libips
2. Add role check after JWT validation in middleware
3. Add `/admin/rbac/` management endpoints
4. Audit logging (who published what, when)

## Dependencies to Add

```toml
# pkg6depotd/Cargo.toml
jsonwebtoken = "10"
```
