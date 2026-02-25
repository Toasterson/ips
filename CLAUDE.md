# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

IPS (Image Packaging System) is a Rust reimplementation of the illumos/OpenIndiana package management system (replacing the Python-based pkg5). It handles software packaging, installation, updates, and dependency resolution for illumos distributions.

**Workspace crates:** libips (core library), pkg6 (client CLI), pkg6repo (repo management CLI), pkg6depotd (HTTP depot server), pkg6recv, pkgtree, specfile, ports, userland, xtask.

## Build and Development Commands

```bash
cargo build                        # Build all crates
cargo check                        # Check without building
cargo nextest run                  # Run tests (CRITICAL: use nextest, not cargo test)
cargo nextest run test_name        # Run specific test
make release                       # Build release + copy to artifacts/
RUST_LOG=libips=trace cargo run    # Run with logging
```

**CRITICAL: Always use `cargo nextest run` instead of `cargo test`.** Tests need process isolation to avoid port conflicts.

## Coding Conventions

### Error Handling (miette + thiserror)

- All error enums derive `#[derive(Debug, Error, Diagnostic)]`
- Each variant has `#[diagnostic(code(...), help(...))]` with actionable guidance
- Subsystem errors chain via `#[error(transparent)] #[diagnostic(transparent)]`
- Return `IpsResult<T>` (alias for `Result<T, IpsError>`) from the top-level API
- Subsystem functions return their own result type (e.g., `RepositoryResult<T>`, `ImageResult<T>`, `FmriResult<T>`)
- Each module defines: `pub type Result<T> = std::result::Result<T, ModuleError>;`
- Diagnostic codes follow the pattern `ips::module::variant` (e.g., `ips::image_error::io`)

### Rust Style

- Use strongly typed idiomatic Rust — express logic in datatypes
- Do error handling — express options for how to handle issues for calling functions
- Use miette's diagnostic pattern to inform the user thoroughly what they need to do
- Never use raw SQL — use rusqlite with proper abstractions
- Edition 2024

## Architecture

### libips (core library)
- `fmri.rs` — FMRI parsing (`pkg://publisher/name@version`)
- `actions/` — Manifest action types (File, Dir, Link, User, Group, etc.) and filesystem executors
- `image/` — Image metadata, catalog queries, installed packages DB (SQLite)
- `repository/` — `ReadableRepository`/`WritableRepository` traits, FileBackend (local), RestBackend (HTTP)
- `solver/` — Dependency resolution via resolvo
- `digest/` — SHA1/SHA2/SHA3 hashing
- `payload/` — Package payload handling
- `publisher.rs` — Publisher configuration
- `transformer.rs` — Manifest transformation/linting
- `depend/` — Dependency generation and parsing

### Key Types
- `Fmri` — Fault Management Resource Identifier (`pkg://publisher/name@release,branch-build:timestamp`)
- `Manifest` — Collection of actions describing a package
- `Image` — Full or Partial image with metadata, publishers, catalogs
- `FileBackend` / `RestBackend` — Repository implementations

## pkg5 Legacy Documentation and Compatibility

We carry the original pkg5 reference documentation in `doc/pkg5_docs/` to guide compatibility with the legacy Python client. Our own docs in `doc/` describe how we've implemented the compatibility layer.

### Reference Specifications (from pkg5)

| Topic | File | What it covers |
|-------|------|----------------|
| **Depot protocol** | `depot.txt`, `depot.rst` | HTTP REST API: `/file/0/{hash}`, `/catalog/0/`, `/manifest/0/{fmri}`, `/search/0/`, `/p5i/0/`, `/publisher/0/`, `/versions/0/` |
| **Retrieval protocol** | `guide-retrieval-protocol.rst` | Client-depot wire protocol, ETag/If-Modified-Since caching, content negotiation |
| **Publication protocol** | `guide-publication-protocol.rst` | Transaction-based publishing (`open`, `add`, `close`) |
| **Repository format** | `guide-repository-format.rst` | On-disk layout: `pkg/`, `file/` (two-level hash dirs), `catalog/`, `trans/`, `updatelog/` |
| **Catalog v1** | `catalog-v1.txt`, `catalog.txt` | Delta-encoded text catalog with `catalog.attrs`, `npkgs`, incremental updates |
| **Actions** | `actions.txt`, `actions.rst` | All 12 action types: file, dir, link, hardlink, depend, service, user, group, driver, license, legacy, set |
| **FMRI & versioning** | `versions.txt`, dev-guide `chpt3.txt` | `pkg://publisher/name@release,branch-build:timestamp`, component ordering |
| **Image types** | `image.txt`, `image.rst` | Full, Zone, User images and their metadata layout |
| **Server API versions** | `server_api_versions.txt`, `client_api_versions.txt` | Protocol version negotiation between client and depot |
| **Signed manifests** | `signed_manifests.txt` | Manifest signature format and verification |
| **Linked images** | `linked-images.txt`, `parallel-linked-images.txt` | Zone/parent-child image constraints |
| **Facets & variants** | `facets.txt` | Conditional action delivery (`variant.arch`, `facet.doc`) |
| **Mediated links** | `mediated-links.txt` | Conflict resolution for symlinks across packages |
| **On-disk format** | `on-disk-format.txt` | Container format proposal for packages |
| **Dev guide** | `dev-guide/chpt1-14.txt` | Full IPS developer guide chapters |

### Our Compatibility Implementation

| Feature | Our doc | Implementation |
|---------|---------|----------------|
| **p5i publisher files** | `doc/pub_p5i_implementation.md` | `FileBackend` generates `pub.p5i` JSON files per publisher for backward compat with `pkg set-publisher -p` |
| **Obsoleted packages** | `doc/obsoleted_packages.md` | Detects `pkg.obsolete=true` during pkg5 import; stores in `<repo>/obsoleted/` with structured metadata |
| **pkg5 import** | `pkg6repo/src/pkg5_import.rs` | Reads `pkg5.repository`, two-level hash dirs, gzip payloads, URL-encoded versions; converts to pkg6 format |
| **Depot REST API** | `pkg6depotd/src/http/` | Serves same HTTP paths as pkg5 depot (`/file/0/`, `/manifest/0/`, `/catalog/0/`, `/publisher/0/`) |
| **libips integration** | `doc/forge_docs/ips_integration.md` | Typed API replacing pkg5 CLI tools (`pkgsend`, `pkgmogrify`, `pkgdepend`, `pkglint`, `pkgrepo`) |
| **Error handling** | `doc/rust_docs/error_handling.md` | miette + thiserror patterns with `ips::module::variant` codes |

### Key Compatibility Surfaces for Legacy Client

The legacy `pkg(1)` Python client expects these from a depot server:
1. **`/versions/0/`** — lists supported operations and protocol versions
2. **`/publisher/0/`** — returns `application/vnd.pkg5.info` (p5i JSON)
3. **`/catalog/0/`** — catalog with `Last-Modified` header, incremental updates
4. **`/manifest/0/{fmri}`** — `text/plain` manifest with `ETag`
5. **`/file/0/{hash}`** — gzip-compressed file content with `ETag` and `Cache-Control`
6. **`/search/0/{token}`** — search returning `index action value package` tuples

## Documentation Maintenance

- Keep `docs/ai/architecture.md` updated when making structural changes. Bump the "last updated" date.
- Create a new timestamped plan in `docs/ai/plans/` before starting a new phase or significant feature.
- Create a new timestamped ADR in `docs/ai/decisions/` when making meaningful technology or design choices. Number sequentially from the last ADR.
- Never delete old plans or decisions. Mark superseded plans with status `Superseded` and link to the replacement.
