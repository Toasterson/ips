# ADR-002: OpenID Connect Authentication for REST API

**Date:** 2026-02-25
**Status:** Accepted

## Context

The REST API (pkg6depotd) and REST client (RestBackend) currently have no authentication. The depot's `auth_check` handler is a stub that only checks for Bearer token presence. The legacy pkg5 used client certificates (x509), which is operationally expensive.

## Decision

Use OpenID Connect (OIDC) for authentication:

- **Depot server** validates JWT access tokens against an OIDC provider's JWKS endpoint
- **REST client** obtains tokens via OIDC flows (device code flow for CLI, authorization code flow for GUI)
- **Token refresh** handled transparently by the client credential manager

### Server Side (pkg6depotd)
- Configure OIDC issuer URL and required scopes in depot config
- Fetch JWKS from `{issuer}/.well-known/openid-configuration` -> `jwks_uri`
- Validate Bearer tokens on protected endpoints (publish, index rebuild)
- Read-only endpoints (catalog, manifest, file, search) remain unauthenticated by default
- Optional: per-publisher access control via JWT claims

### Client Side (libips RestBackend)
- Add `CredentialProvider` trait to RestBackend
- Implement OIDC device code flow for CLI (user opens browser, enters code)
- Token storage in image metadata directory (encrypted at rest)
- Automatic refresh before expiry

## Consequences

- Modern auth infrastructure, compatible with Keycloak/Dex/Auth0/etc.
- No client certificate management burden
- Publisher-level access control possible via scopes/claims
- Requires OIDC provider deployment for secured repos (optional — unsecured repos still work)
