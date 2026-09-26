# Atomic p5p imports

The v1 ingest path takes one `.p5p` archive, typically one per CI run, and
publishes it into a live repository:

```sh
# su pkg6srv -c 'RUST_LOG=info /usr/bin/pkg6repo import-pkg5 \
      -s /var/tmp/run-1234.p5p -d /var/pkg6depotd/repo'
```

`pkg6repo` logs at `warn` by default. With `RUST_LOG=info` it also prints
the outcome, the generation, the archive SHA-256 and the added and skipped
FMRIs.

| Option | Meaning |
|--------|---------|
| `-s`, `--source` | A `.p5p` archive or a pkg5 directory repository. |
| `-d`, `--destination` | The repository the depot serves. |
| `-p`, `--publisher` | Import only this publisher. Repeatable. Default: every publisher in the source. |
| `--adopt-existing` | Convert a populated repository from the pre-generation layout. See [Adopting an existing repository](#adopting-an-existing-repository). |
| `--max-archive-entries` | Limit on files and directories unpacked. Default 20,000,000. |
| `--max-extract-bytes` | Limit on unpacked file bytes. Default: the archive's own size. |
| `--max-import-manifests` | Limit on selected manifest files held by one import. Default 10,000. |
| `--max-import-manifest-bytes` | Limit on selected manifest bytes held by one import. Default 64 MiB. |

## All or nothing

An archive can hold several publishers. Either every package in it becomes
visible in one step, or none does.

1. **Validate.** The archive is unpacked in-process into a scratch
   directory. Only regular files and directories with plain relative paths
   are accepted: links, devices, absolute paths or `..` reject the
   archive. For a V4 directory source, any link or special file under
   `publisher/` rejects it. A flat V3 source uses top-level `cfg_cache`,
   `pkg/` and `file/`; links and special files there reject it. V4
   manifests must carry a versioned `pkg.fmri`. V3 manifests may instead
   take their identity from the encoded path and `cfg_cache` publisher;
   an explicit `pkg.fmri` must match. Every
   payload referenced by `file`, `license` and `signature` actions must be
   present and match its digest. The selected manifest count and bytes must
   fit the import budget, or the whole batch is rejected.
2. **Stage.** Payloads are stored under the names pkg5 clients request.
   Manifests, v1 catalog parts and SQLite shards for all publishers are
   written to `generations/.staging-<id>` and fsynced.
3. **Switch.** The staging directory is renamed to `generations/<id>`.
   Then `generations/current` is replaced by `rename(2)` of a new symlink.
   That rename is the single commit point.

Any problem in step 1 rejects the batch before anything is written. A
crash before step 3 leaves the old generation published. The next writer
cleans up the leftovers.

The stable paths `publisher/<pub>/{pkg,catalog,catalog2}` are symlinks
through `generations/current`. The depot and the pkg5 protocol see
unchanged paths. The current generation and the two before it are kept;
the next writer deletes older ones. A reader that already resolved an old
generation keeps a complete snapshot.

## Idempotency and conflicts

| Situation | Result |
|-----------|--------|
| Same archive (same SHA-256) imported again | No-op, reported as already imported. Also covers a crash after the switch but before your CI saw success. |
| Package whose exact FMRI is already published with identical bytes | Skipped; the rest of the batch is imported. |
| Directory source whose packages are all published | No-op, "nothing to import". |
| Directory retry finds a missing payload of an identical published package | Restores the verified blob and reports `Repaired`; no new metadata generation is made. |
| Exact FMRI already published with **different** bytes | Whole batch rejected with `ips::pkg5_import::conflict`, listing every conflicting FMRI. Nothing is written. |
| Same exact FMRI twice in one batch with different bytes | Rejected the same way. |
| Another writer holds the lock | Waits for it. |

A published FMRI is never replaced. To change a package, publish it
under a new version.

The receipt `receipts/<sha256>.json` lists the added and skipped FMRIs. It
is written inside the generation it publishes. So a receipt exists exactly
when the import is committed, and a retry can trust it. If you remove a
publisher, every receipt is narrowed to drop it, so re-importing the same
archive publishes that publisher again.

**For CI:** retry the same file on any failure. Exit status 0 means the
content is published. A conflict is permanent until the package version
changes.

## Payloads

Payloads are stored as pkg5 clients install them from `/file/`: one gzip
member whose content matches the digest, and whose compressed bytes match
any `chash` or `pkg.chash.*` the manifests pin.

| Source payload | Result |
|----------------|--------|
| Already in that form | Stored byte for byte. |
| Uncompressed or multi-member | Recompressed, but only if no manifest pins its compressed bytes. Otherwise the batch is rejected. |
| Copy already in the repository | Reused only if it passes the same check. An LZ4 or other non-gzip copy, or a `chash` mismatch, rejects the batch. Existing copies are never replaced. |

## Every repository writer

All writers take an exclusive `flock(2)` on `<repo>/.pkg6-writer.lock`, so
they serialise:

- imports;
- `rebuild` and `refresh`;
- publisher add and remove;
- configuration edits.

On a generation-managed repository:

- `pkg6repo rebuild`/`refresh` build a new generation and switch to it.
- `add-publisher`/`remove-publisher` publish a new generation. Removal
  keeps the publisher's payload store.
- In-place writers fail with `ips::generation::managed_repository` before
  changing anything: `pkg6recv`,
  `restore-obsoleted` and transaction-based publishing. Receive into a
  separate unmanaged repository, archive it to `.p5p` and import that.

## Adopting an existing repository

- **Empty repository:** converted to generations on the first import.
- **Populated repository in the old layout:** the import refuses with
  `ips::generation::adoption_required`. Rerun it with `--adopt-existing`
  once.

Adoption hard-links the live directories into generation zero and
replaces each directory with its link. `rename(2)` cannot replace a
directory with a symlink, so each path is absent for microseconds between
two renames. Stop the depot for this step, or accept that clients may
retry.

A writer also stops before changing anything if it finds live metadata
directories it cannot account for:

- **`ips::generation::unexpected_live_directory`:** a real directory of a
  published publisher that differs from the generation.
- **`ips::generation::unregistered_live_directory`:** a real directory of
  a publisher that is neither published nor configured.

Inspect and move such directories by hand.

## Known limits

- A manifest whose only payload digest is SHA-2 `pkg.content-hash`,
  without a SHA-1 `hash`, is rejected. OpenIndiana's default publication
  still emits `hash`.
- Each import rebuilds the catalogs of the changed publishers in full. It
  also costs one hard link per published manifest.
- The current importer holds one whole batch in memory. The default
  10,000-manifest and 64 MiB manifest-byte limits reject oversized sources
  before package content is published. Explicit overrides are possible,
  but neither limit splits a batch or guarantees a fixed peak RSS.
- Payload temporaries `.<hash>.import` left by a crash inside
  `publisher/<pub>/file/` are harmless and never served, but are not
  cleaned up yet.
- **Not verified:** crash safety is tested with injected failure points,
  not with real power loss on ZFS.
- **Pending:** import telemetry. The metrics exist but are not recorded;
  see [OpenTelemetry](telemetry.md).
