# Repository Retirement, Protected Snapshots and Import Exclusions (v1)

**Date:** 2026-09-23
**Status:** Active
**Issue:** [#39](https://code.aopc.cloud/OpenIndiana/ips/issues/39) (part of #24; command audit #40)
**Builds on:** [ADR-005](../decisions/005-atomic-pkg5-import-via-repository-generations.md)
**Decision:** [ADR-006](../decisions/006-retirement-snapshots-and-import-exclusions.md)

## Goal

First stage of two-stage retirement: an operator hides selected superseded
package versions from catalogs and search while exact manifest and payload
fetches keep working. Selected historical snapshots stay catalog- and
solver-visible. Import exclusions are explicit and cannot break the protected
set. No purge, no automatic deletion, no S3.

## Slice

1. `libips::repository::retention`
   - `policy`: KDL policy (`<repo>/policy.kdl` or `--policy`): `retirement`
     (enabled, inactivity, purge-grace), `keep` rules (exact or regex),
     `import { exclude ... }`, `snapshot` sets. Keep files in text, KDL or
     JSON. Everything is validated before any mutation.
   - `version`: pkg5 version order and dependency matching (from the local
     OpenIndiana pkg5 `version.py`).
   - `access`: durable access accounting, written by the depot
     (append-only day logs under `<repo>/retention/access/`), folded by the
     writer.
   - `universe`: dependency/snapshot closure over a generation's manifests.
   - `plan`: plan, apply (recheck under the writer lock, atomic generation),
     restore, status.
2. Generation integration: retirement state and audit records live inside
   each generation; catalog builds leave hidden versions out; state is
   carried into every later generation (imports, rebuilds, publisher removal).
3. Import exclusions in `import-pkg5` with preflight against snapshots and
   the dependency closure of imported packages; excluded FMRIs are recorded in
   the receipt.
4. Depot: manifest and payload GETs record qualifying accesses.
5. CLI: `pkg6repo retire plan|apply|restore|status|check-policy`,
   `pkg6repo import-pkg5 --policy`.
6. Tests: hiding, exact access, clock/access, snapshots, exclusions,
   idempotence, failure injection, crash/reload; depot HTTP; CLI.
7. Docs: ADR-006, man pages `pkg6repo-retire(8)`, `pkg6-policy(5)`,
   `doc/obsoleted_packages.md`.

## Done (2026-09-23)

All of the slice above, with tests in `libips/src/repository/retention/`
(unit), `libips/tests/retirement.rs`, `pkg6depotd/tests/retirement_http_tests.rs`
and `pkg6repo/tests/retire_cli.rs`. `pkg6repo` renders errors as miette
diagnostics. Man pages live in `man/` until packaging (`packaging/ips/`, in
the release-pipeline work) ships them.

## Review fixes (2026-09-23)

An independent review found release blockers; all are fixed in this slice
(ADR-006, decisions 11-15):

- Access history fails closed: chained heartbeats per depot, gaps for
  crashes, failed writes, deleted/truncated/unreadable records, stale depots
  make history unproven; `retire close-depot` for a depot that is gone.
- `--now` removed; `retention::clock` is the wall clock, pinnable only by
  unit tests and the libips-only `test-clock` feature.
- Apply's final access check runs under the access fence right before the
  switch (`GenerationStore::commit_gated`); depots queue records while it is
  held.
- Unreadable versions/dependencies block, unsatisfied snapshot requirements
  are reported, import exclusions check already-published dependents,
  restore restarts the inactivity clock.

Tests: `access::tests` (unit), `universe::tests` (unit),
`libips/tests/retirement.rs`, `pkg6depotd/tests/retirement_http_tests.rs`,
`pkg6repo/tests/retire_cli.rs`.

## Remaining (not in this slice)

See ADR-006 "Consequences" and the final report: purge (second stage),
validation with real OpenIndiana pkg5 clients from 2020 ISOs, ISO inventory
tooling for snapshots, payload access attribution for shared payloads,
reverse-proxy cache effects on access accounting.
