# ADR-004: v1 Public HTTP Is Read-Only; pkg5 Obsoletion Stays in the Catalog

**Date:** 2026-09-23
**Status:** Accepted
**Amends:** [ADR-002](002-openidconnect-rest-authentication.md) (for v1 only)
**Issues:** #24, #25, #40

## Context

The maintainer decided (#24/#25/#40, 2026-09-22) that in v1 the public HTTP
interface is read-only. Imports, retirement/purge and repository
administration run as local commands only. Remote publication and OIDC/RBAC
are deferred.

The depot still routed two non-serving endpoints:

- `GET /{publisher}/index/0/{command}`: `refresh` called
  `FileBackend::rebuild(pub, no_catalog=true, no_index=false)`. With current
  libips that call writes nothing, but it took the backend mutex, and it would
  start writing as soon as `no_index` is honoured.
- `POST /admin/auth/check`: with OIDC unconfigured, it returned `200 allow`
  for any Bearer token.

pkg5's read-only depot (`REPO_OPS_READONLY`, `src/modules/server/depot.py`)
serves only `versions`, `search`, `catalog`, `info`, `manifest`, `file`,
`p5i`, `publisher` and `status`. It has no `index`, `admin` or publication
operations.

Separately, `pkg6repo obsolete-package` moved a manifest out of `pkg/` into
the pkg6-only `obsoleted/` store and rebuilt the catalog. That is neither pkg5
obsoletion nor retirement. In pkg5, obsoletion is a published manifest with
`pkg.obsolete=true` that stays in the catalog. `Catalog.add_package`
(`src/modules/catalog.py`) puts `pkg.obsolete`, `pkg.renamed`, `variant*`,
`facet*` and `pkg.depend.*` in `catalog.dependency.C`. The client solver
(`pkg_solver.__fmri_loadstate`) and image package state read the flags only
from that part. Our catalog writer put them in `catalog.summary.C`, so legacy
clients never saw them.

## Decision

1. The public router serves only package and search content, the web UI and
   `/admin/health`. `index/0`, `admin/auth/check` and their handlers
   (`handlers::index`, `admin::auth_check`, `DepotRepo::rebuild_index`) are
   removed. `/versions/0` keeps not advertising `index`/`admin`.
   `pkg6depotd/tests/read_only_http_tests.rs` asserts that pkg5 mutating
   request shapes return 404/405 and that neither they nor the read routes
   change repository files.
2. `pkg6repo obsolete-package` is removed. The `*-obsoleted` store commands
   stay as inspection and maintenance tools. `cleanup-obsoleted` and
   `restore-obsoleted` need review under #39/#40 (unchecked permanent
   deletion; non-atomic restore).
3. The catalog writer places attributes in catalog parts the way pkg5 does,
   so `pkg.obsolete`/`pkg.renamed` reach legacy clients through
   `catalog.dependency.C`.

## Consequences

- Operators rebuild catalogs and indexes with local `pkg6repo rebuild` or
  `refresh`, never over HTTP.
- ADR-002's "protected endpoints (publish, index rebuild)" does not apply to
  v1. OIDC wiring returns with authenticated publication after v1. The
  `AuthState` middleware and its tests remain in place for that work.
- Catalogs rebuilt after this change move `pkg.obsolete`/`pkg.renamed` (and
  `pkg.depend.*`) from the summary part to the dependency part. Existing
  repositories need a catalog rebuild to become visible to legacy clients.
- Still open: the pkg5 importer diverts `pkg.obsolete=true` packages into the
  obsoleted store instead of the catalog (#38). Update-log entries written
  during a catalog rebuild reuse the same per-part action lists, so they
  follow the same placement.
