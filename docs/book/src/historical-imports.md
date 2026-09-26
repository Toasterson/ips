# Importing historical content locally

To seed a v1 repository with older OpenIndiana content, use the same local
importer as CI: `pkg6repo import-pkg5` with a `.p5p` archive or a pkg5
directory repository. There is no remote or HTTP import, and no pull from
another depot.

## Preparing the source

Produce a pkg5 archive with the pkg5 tools on a host that can reach the old
origin. For example, one `entire` version and its dependencies:

```sh
$ pkgrecv -s https://pkg.openindiana.org/hipster -d /var/tmp/hipster-2020.10.p5p \
    -a -r 'pkg://openindiana.org/entire@2020.10'
```

Alternatively, use a pkg5 filesystem repository. Version 4 repositories
contain `pkg5.repository` and `publisher/`; flat version 3 repositories
contain `cfg_cache`, `pkg/` and `file/`. The V3 publisher comes from
`cfg_cache`, and source manifest bytes are preserved. The separate
OpenSolaris `legacy` V3 repository has publisher `opensolaris.org`; it is
not an OpenIndiana Hipster archive. The final repository must contain it
as a separate publisher.

## Importing

```sh
# zfs snapshot rpool/pkg6depotd@before-2020.10
# su pkg6srv -c 'RUST_LOG=info /usr/bin/pkg6repo import-pkg5 \
      -s /var/tmp/hipster-2020.10.p5p -d /var/pkg6depotd/repo'
```

The rules of [Atomic p5p imports](importing.md) apply unchanged:

- **Batches are atomic.** Each archive or directory import is one batch.
  The current importer rejects a source above 10,000 selected manifests
  or 64 MiB of selected manifest bytes by default. Override those limits
  only after checking memory for the entire batch. A resumable bounded
  historical importer is still required for whole large NFS sources.
- **Order does not matter.** Importing older versions after newer ones is
  fine: the catalog holds every published version, and pkg5 version
  ordering picks the newest.
- **Overlap is safe.** Versions already in the repository with identical
  bytes are skipped. A byte difference for the same exact FMRI rejects the
  whole archive. Such a difference is typically a republished manifest
  that kept its timestamp, and the conflict error names it.
- **Directory sources are re-runnable.** A second run over the same
  directory reports "nothing to import" if it is complete, or restores
  verified payloads missing from already published packages without a new
  catalog generation. Receipts are keyed by archive SHA-256, so only
  `.p5p` sources get one.
- **Use `-p` to choose publishers.** Repeat it to import only some of
  the source's publishers.
- **Inspect limit failures.** `--max-archive-entries` and
  `--max-extract-bytes` bound archive unpacking. The independent
  `--max-import-manifests` and `--max-import-manifest-bytes` bound selected
  manifest input; exceeding them fails with
  `ips::pkg5_import::manifest_budget` before package publication.

Do not divide a source with import exclusions into independent batches and
assume each batch's dependency check protects the whole history. A package
in a later batch may require one excluded earlier. Exclusion preflight for
bounded historical imports must cover the entire selected source before the
first batch is published.

## Checking the result

```sh
$ /usr/bin/pkg6repo list -s /var/pkg6depotd/repo -p openindiana.org | head
$ pkg list -afv -g https://pkg.example.org 'entire'
```

`pkg6repo list` shows obsolete versions too, as `pkgrepo list` does.

## Historical content pitfalls

- **SHA-2-only manifests.** A manifest whose only payload digest is
  `pkg.content-hash` is rejected. Content published with the default
  OpenIndiana settings carries a SHA-1 `hash` and imports.
- **pkg6-written repositories.** Payload copies that pkg6 stored
  LZ4-compressed, or otherwise not as a single gzip member, cannot be
  served to pkg5 clients as-is. The import rejects the batch rather than
  replace them.
- **Other ingest tools.** `pkg6recv` refuses a generation-managed
  repository. Use them only against a separate,
  unmanaged staging repository.
- **Catalog visibility.** Every imported version stays in the catalog
  until it is retired. It is solver-visible, and pkg5 clients download a
  bigger catalog. `pkg6repo retire` hides superseded versions, snapshot
  protection keeps selected historical sets installable, and
  `import { exclude ... }` leaves packages out of an import. See
  [Retention, snapshots and exclusions](retention.md).
- **Not verified:** install and update with real 2020-era OpenIndiana
  clients against imported history.
