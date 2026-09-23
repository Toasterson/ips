# ADR-007: Atomic pkg5 Import via Immutable Repository Generations

**Date:** 2026-09-23
**Status:** Accepted
**Issues:** #38 (implementation), #26 (local batch transaction model), #24
**Related:** [ADR-004](004-v1-read-only-public-http-and-obsoletion.md)
**Amended by:** [ADR-009](009-v1-pkg6repo-command-surface.md) — `pkg6repo import-archive` is removed; mentions of it below are historical and `pkg6recv` is the remaining receive path

## Context

The v1 ingest contract (#38, #26, maintainer decisions of 2026-09-22):

- CI delivers one `.p5p` per run, possibly with several publishers. Every
  package becomes visible in one transition or none does.
- Retrying an identical archive succeeds as a no-op, including after a crash
  between commit and acknowledgement. Different content for an existing exact
  FMRI rejects the whole batch. Identical existing packages are skipped.
- Reads stay available throughout imports and catalog rebuilds. Readers see
  the previous complete catalog until the replacement is ready, then switch
  atomically by `rename(2)`, keeping the existing on-disk catalog pathnames.
- Existing SQLite/shard/sidecar files must not be assumed to switch
  atomically by renaming a single file.

Reader-visible state of a publisher is spread over many files:
`publisher/<pub>/catalog/` (pkg5 v1 parts), `publisher/<pub>/catalog2/`
(`active.db`, `obsolete.db`, `fts.db`, their content-addressed copies and
`catalog.attrs`), `publisher/<pub>/pkg/` (manifests) and
`publisher/<pub>/file/` (payloads). The depot resolves these pathnames per
request. It opens SQLite shards read-only per request, and they are written
in rollback-journal mode and closed before publication, so no WAL/SHM state
exists.

The previous importer committed each package in turn, rebuilt the catalog in
place, handled only one publisher per run, re-hashed payloads under SHA-256
names that pkg5 clients never request, and diverted `pkg.obsolete` packages
into the side store.

## Decision

1. **Generations.** Manifests, v1 catalog parts and SQLite shards of all
   publishers live in immutable generation directories
   `generations/<id>/publisher/<pub>/{pkg,catalog,catalog2}`. The stable
   pathnames `publisher/<pub>/{pkg,catalog,catalog2}` are symlinks through
   `generations/current`, which names the published generation.
2. **Single commit point.** A writer stages `generations/.staging-<id>` on
   the same filesystem, fsyncs new files and every directory, renames it to
   `generations/<id>`, then replaces `generations/current` by `rename(2)` of
   a fresh symlink and fsyncs `generations/`. That rename publishes all
   publishers at once.
3. **Sharing without mutation.** A new generation hard-links the parent's
   manifests. Catalogs of changed publishers are built from scratch in the
   staging tree. Nothing ever writes into a file of a committed generation.
   New content is always a new inode, so a reader that resolved the old
   generation keeps a complete snapshot. The two previous generations are
   retained, and older ones are deleted by the next writer.
4. **Shared payloads.** Payloads stay in `publisher/<pub>/file/` under the
   names manifests reference, in the form pkg5 clients install from
   `/file/0/<hash>`: a single gzip member (pkg5 decompresses only the first)
   whose content has that digest and whose compressed bytes match every
   `chash`/`pkg.chash.*` the manifests pin. A source payload in that form is
   stored byte for byte. An uncompressed or multi-member one is recompressed
   into a single member, but only if no manifest pins its compressed bytes;
   otherwise the batch is rejected. A copy the repository already serves for
   the publisher (its own, else the global `file/` store) is reused only if
   it passes the same check (so an LZ4 pkg6 copy, a non-gzip copy or a
   `chash` mismatch rejects the batch) and is never replaced or shadowed by
   a new publisher copy, because pkg6 packages may reference the same
   digest. Payloads are stored (fsync + rename) before the switch. An
   unreferenced payload left by a crash is harmless and reused on retry.
5. **Receipts.** The archive's SHA-256 is its identity. The receipt
   (`receipts/<sha256>.json`, with added and skipped FMRIs) is written inside
   the generation it publishes and carried into later generations, so a
   receipt exists exactly when the import is committed. Removing a publisher
   narrows every receipt naming it (and drops receipts left empty), so
   re-importing the same archive publishes that publisher again.
6. **Writer lock and recovery.** Every repository writer holds an exclusive
   `flock(2)` on `<repo>/.pkg6-writer.lock`: imports, rebuild/refresh,
   adoption, publisher add/remove, configuration edits, and the in-place
   writers of unmanaged repositories (`Transaction::commit`, legacy rebuild,
   catalog part/update-log writers). On acquiring it, a generation writer
   removes staging leftovers, adopts live real directories and reconciles.
   Only publishers named by the published generation or the configuration
   are adopted. A real `pkg/`, `catalog/` or `catalog2/` of any other
   directory under `publisher/` would stay served in place of a generation
   that later publishes that name, so the writer fails with
   `ips::generation::unregistered_live_directory` before changing anything
   (first adoption included). A real directory of a published publisher is
   accepted only if the generation already holds all of its manifests (the
   leftover of a crash during adoption) and is then replaced by its link;
   anything else fails with `ips::generation::unexpected_live_directory`.
7. **The generation defines the publishers.** Each `generation.json` records
   the publisher list and default publisher. On a managed repository
   `FileBackend::open` takes both from the generation `generations/current`
   names (keeping the configured order and default where they agree), so
   publisher discovery, the default publisher and unscoped search change
   with the same `rename(2)` as the catalogs. After every switch and during
   recovery the configuration file is rewritten to match, for legacy readers
   of the file only. Links of publishers that are not
   published are removed. `add_publisher`/`remove_publisher` on a managed
   repository publish a new generation with the publisher added (empty) or
   removed, so a removed publisher cannot be resurrected by a later rebuild.
   Removal keeps the publisher's payload store. Receivers (`pkg6recv`,
   `import-archive`) and `restore-obsoleted` check this before their first
   mutation, so a refused command neither adds a publisher nor loses a
   side-store record.
8. **In-place writers are rejected on managed repositories.** Transactions
   (`publish`, `pkg6recv`, `import-archive`, `restore-obsoleted`, the publish
   API), `get_catalog_manager` and the catalog part/update-log writers fail
   with `ips::generation::managed_repository` before mutating anything. They
   re-check under the writer lock, so a transaction that began before an
   import adopted the repository cannot write into the live generation.
   Batch ingestion into a managed repository is `import-pkg5`.
9. **Adoption of existing repositories needs consent.** An unmanaged
   repository without manifests is converted automatically. A populated one
   needs `import-pkg5 --adopt-existing` (`Pkg5ImportOptions::adopt_existing`).
   The first writer copies (hard-links) the live directories into generation
   zero and replaces each directory with its link. `rename(2)` cannot replace
   a directory with a symlink, so each path is absent between two renames.
   This is a one-time window measured in microseconds.
10. **Validation before visibility.** Archives are unpacked in-process (the
   `tar` crate, not tar(1)); only regular files and directories with plain
   relative paths are accepted, so links, devices, absolute paths or `..`
   reject the archive before anything is written outside the scratch tree.
   Directory sources are refused if their `publisher/` tree holds any link
   or special file. Unpacking never writes more file data than the archive
   holds (a larger size claim is refused before writing) and stops at
   20 million entries by default; `--max-archive-entries` and
   `--max-extract-bytes` adjust both with an explicit
   `ips::pkg5_import::archive_limit` error. The importer parses every manifest and
   requires a versioned `pkg.fmri` matching its publisher directory, with a
   package name other than `.`/`..`. It
   locates and verifies every payload referenced by file, license and
   signature actions (including `chain`). It detects exact-FMRI conflicts
   (byte comparison) against the published generation and within the batch.
   Any problem rejects the batch before anything is written.
11. **pkg5 semantics.** Manifests are stored byte for byte. `pkg.obsolete` and
   `pkg.renamed` packages are published into the main catalog like any other
   package. Nothing is retired or hidden. In the SQLite shards, obsolete
   versions go to `obsolete.db` only, not to `active.db` (installable) or
   `fts.db` (search). `list_packages` lists both, like `pkgrepo list`.
12. **Readers follow the switch.** `pkg6depotd` reloads its backend when
   `generations/current` points at a new generation (or `pkg6.repository`
   is replaced), so `/publisher/0`, `/publisher/1`, per-publisher discovery
   and unscoped search follow the switch without a restart and without
   waiting for the configuration write. New catalog files get an mtime strictly later, in
   whole seconds, than any catalog published before (a repository-wide
   high-water mark in `generation.json`, which also covers a publisher
   removed and re-added), so pkg5's
   If-Modified-Since revalidation of `catalog.attrs` never gets a false 304.
   Publisher, digest and file-name path segments are validated before they
   name a file.
13. **Rebuild.** `FileBackend::rebuild`/`refresh` on a managed repository
    build a new generation and switch to it. There is still no HTTP rebuild
    (ADR-004).

## Consequences

- Stable pathnames: the pkg5 depot protocol, `pkg6depotd` and pkg6 readers
  are unchanged.
- A pkg5 refresh that fetches `catalog.attrs` from one generation and a part
  from the next fails pkg5's SHA-1 part validation. The client does not
  accept a mixed or partial batch, and its retry sees the new catalog. pkg5's
  protocol names parts without a version, so no server layout can make a
  refresh that spans a switch succeed without a retry.
- Each import costs O(published manifests) hard links plus directory fsyncs,
  and a full catalog rebuild of the changed publishers. An incremental
  catalog merge is follow-up work.
- `pkg6recv`, `import-archive`, `publish`, `restore-obsoleted` and the
  publish API cannot write into a managed repository. Use them against an
  unmanaged repository and import its `.p5p`, or add a generation-backed
  batch writer when these paths are needed (post-v1).
- Unscoped search keeps pkg5's meaning: the default publisher.
- A manifest whose only payload digest is SHA-2 `pkg.content-hash` is
  rejected (the action parser cannot read it). OpenIndiana's default
  publication still emits the SHA-1 `hash`.
- Retirement and purge (#39/#40) must go through `GenerationStore`, so that
  they serialise with imports and publish atomically.
