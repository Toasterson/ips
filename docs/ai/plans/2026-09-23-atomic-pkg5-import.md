# Atomic Local pkg5 Import (v1 ingest)

**Date:** 2026-09-23
**Status:** Active
**Issue:** [#38](https://code.aopc.cloud/OpenIndiana/ips/issues/38) (part of #24, transaction model #26)
**Decision:** [ADR-005](../decisions/005-atomic-pkg5-import-via-repository-generations.md)

## Goal

`pkg6repo import-pkg5 -s run.p5p -d <repo>` ingests one CI-delivered,
possibly multi-publisher `.p5p` as one atomic batch while the depot keeps
serving reads.

## Done in this slice

- `libips::repository::generation`: writer lock, generation staging, durable
  finalisation, atomic `current` switch, recovery (leftovers, adoption of
  pre-generation directories, post-switch reconciliation), retention GC,
  import receipts.
- `libips::repository::pkg5_import`: archive SHA-256 identity, full
  manifest and payload validation, exact-FMRI conflict detection, identical
  package skipping, payload storage under pkg5 names, byte-exact manifests,
  `pkg.obsolete` kept in the main catalog. Failure injection at
  `AfterPayloads`, `BeforeSwitch` and `AfterSwitch`.
- `FileBackend::rebuild`/`refresh` publish a new generation on managed
  repositories. Configuration files are written atomically.
- Review fixes (2026-09-23): obsolete versions out of `active.db`/`fts.db`;
  in-place writers take the writer lock and are rejected on managed
  repositories; publisher add/remove through generations and a
  generation-defined configuration; depot configuration reload; strictly
  increasing catalog mtimes; `--adopt-existing` gate; receipt merging and
  `-p` deduplication; pkg5 V0 payload layout; publisher-name and request
  path validation; fsync of new payload and publisher directories.
- Final review fixes (2026-09-23): receipts narrowed on publisher removal;
  in-process validating `.p5p` unpacker and link-free directory sources;
  receive/import-archive and restore-obsoleted refuse managed repositories
  before mutating (restore removes the side-store record only after commit);
  `.`/`..` package names rejected; repository-wide catalog mtime high-water
  mark.
- Publisher discovery and the default publisher derive from the current
  generation (single switch), tested with a live depot across
  before/after-switch failpoints for publisher addition and removal.
  Extraction entry/byte limits.
- Payload wire compatibility: stored and reused payloads must be a single
  gzip member with the right content digest and any pinned `chash`;
  unpinned uncompressed or multi-member sources are recompressed, existing
  LZ4/non-gzip/mismatched copies reject the batch and are never replaced.
- Live-directory fail-closed checks: a real metadata directory of a
  published publisher that diverges from the generation
  (`unexpected_live_directory`), or of a publisher neither published nor
  configured (`unregistered_live_directory`), stops every writer before any
  mutation, on first adoption and later imports alike. Crash leftovers of
  adoption are still replaced by their links.
- `pkg6repo import-pkg5`: imports all publishers by default, `-p` is
  repeatable, and it exits non-zero with named conflicts.
- Tests: `libips/tests/atomic_pkg5_import.rs` (crash points, retry,
  conflicts, missing or corrupt payloads, reader snapshots and a pkg5-style
  refresh across the switch, legacy adoption, rebuild, lock, retention,
  payload wire form, unexpected and unregistered live directories),
  `pkg6depotd/tests/atomic_import_http_tests.rs` (HTTP catalog, manifest,
  file and search across a crash and a switch),
  `pkg6repo/tests/import_pkg5_cli.rs`.

## Remaining

- Validate with the real OpenIndiana pkg5 client (`pkg refresh`,
  `pkg install`, `pkg update`) against `pkg6depotd` during an import and
  across a crash. The tests here only emulate pkg5's catalog validation.
- Real power-loss testing on illumos/ZFS: the tests simulate crashes by
  aborting at fail points, not by losing unsynced writes.
- Incremental catalog merge instead of a full rebuild per changed publisher,
  and cheaper generation cloning for very large repositories.
- A generation-backed batch writer for `pkg6recv`/`import-archive`/publish,
  which are rejected on managed repositories today. The obsoleted-store
  commands other than restore (which uses a transaction) write only to
  `obsoleted/` and are unaffected (#39/#40).
- SHA-2-only payload references (`pkg.content-hash` without `hash`).
- Clean up `.<hash>.import` payload temporaries left by a crash inside
  `publisher/<pub>/file/`. They are harmless and never served under a
  payload name.
- Catalog `created` changes on every rebuild, so pkg5 clients do a full
  catalog download after each import. This is unchanged behaviour; update
  logs for incremental refresh are follow-up work.
