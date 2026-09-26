# ADR-013: Flat pkg5 Sources and an In-Memory Import Budget

**Date:** 2026-09-26
**Status:** Accepted
**Related:** [ADR-007](007-atomic-pkg5-import-via-repository-generations.md), [historical import #53](https://code.aopc.cloud/OpenIndiana/ips/issues/53)

## Context

The historical NFS export has V4 OpenIndiana Hipster repositories and a
separate flat V3 OpenSolaris repository named `legacy`. The latter declares
`opensolaris.org` in `cfg_cache`, has top-level `pkg/` and `file/`, and some
manifests omit the `pkg.fmri` action. Its 2,108,239 manifest files cannot be
read as one batch into oirepo6's measured 4 GB zone memory limit. The
existing importer keeps all source manifests and their payload references in
memory while validating one atomic batch.

## Decision

- Recognize V3 only from its own flat directory layout and configuration.
  Reject a source mixing V3 and V4 markers. Never treat `legacy` as a Hipster
  source or assign its packages to `openindiana.org`.
- Decode V3 manifest path components strictly. The path and configured
  publisher supply the FMRI when the manifest lacks `pkg.fmri`; an explicit
  FMRI must agree with them. Preserve source manifest bytes. Use the same
  payload digest checks and generation publication as V4.
- Restore missing payloads of already-published identical packages when a
  directory source is retried. A repair-only retry writes the verified
  shared-store blobs and reports repair without publishing a duplicate
  metadata generation. An archive with a current receipt remains a fast
  `AlreadyImported` result without a payload scan.
- Limit the current whole-batch importer to 10,000 selected manifest files
  and 64 MiB of manifest bytes by default. A caller may supply finite,
  positive explicit overrides after checking available memory. Crossing a
  limit rejects the entire batch before package content is published; it
  does not silently partition a `.p5p` or directory source.
- Implement resumable bounded historical imports, with full-source policy
  preflight, separately under #53. The current budget is an operational
  guard, not a guarantee of a fixed peak RSS or a substitute for that work.

## Consequences

The final repository must host the OpenSolaris `legacy` source under
`opensolaris.org` alongside OpenIndiana Hipster publishers; they remain
distinct sources and publisher identities. Large NFS sources need a bounded import path before
they can be migrated on oirepo6. CI's one-p5p atomic import contract remains
unchanged; an over-budget CI archive fails explicitly and must not be split
without a separate publication decision.
