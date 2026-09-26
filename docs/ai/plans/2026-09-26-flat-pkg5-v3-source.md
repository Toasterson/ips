# Flat pkg5 V3 source import

**Date:** 2026-09-26
**Status:** Implemented format support; bounded historical import pending
**Scope:** the `legacy` OpenSolaris repository on the read-only NFS export,
mandatory in the final repository under `opensolaris.org` and kept distinct
from OpenIndiana Hipster sources.

The observed `legacy` repository has top-level `cfg_cache`, `pkg/`, `file/`,
and `catalog/`. Its configuration declares `opensolaris.org`. Some manifest
files lack a `pkg.fmri` action; their comment header omits the timestamp in
the percent-encoded version filename. The import must preserve the manifest
bytes and identify the package from its path and configuration.

Extend the existing importer to recognize this layout explicitly, reject
ambiguous mixed V3/V4 trees and unsafe paths, and route manifest and payload
scanning through the same digest verification and generation publication as
V4. For V3, derive an absent FMRI from strictly decoded path components and
the configured publisher. Check any explicit manifest FMRI against that path.
Existing V4 and p5p behavior remains intact. Test independent publishers,
malformed configuration/paths, symlinks, missing payloads, and retry.

The NFS source has 2,108,239 manifest files; the current importer holds a
whole batch in memory and cannot safely import this source in one run on the
4 GB oirepo6 zone. The current all-at-once importer therefore rejects
selected batches above 10,000 manifests or 64 MiB of manifest bytes by
default; explicit overrides do not split a batch. A later bounded,
resumable import path ([#53](https://code.aopc.cloud/OpenIndiana/ips/issues/53)) must preserve
per-batch atomicity and validate import exclusions against the full source
before publishing any batch. V3 parsing support alone does not authorize or
make the full historical import feasible. No full import is planned in this
code pass.
