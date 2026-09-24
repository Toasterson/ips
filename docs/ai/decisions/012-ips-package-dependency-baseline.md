# ADR-012: Resolve IPS Package Dependencies Against a Baseline Image

**Date:** 2026-09-24
**Status:** Accepted
**Related:** [ADR-005](005-release-pipeline-and-ips-packaging.md),
[ADR-011](011-manual-illumos-release-assets.md)

## Context

`pkgdepend resolve` writes the version of each dependency that is installed in
the image it resolves against as the minimum version. `build-ips-package.sh`
resolved against the build host. `v1.0.0-rc.11` was built on oibuild, which
is newer than the target, so the package required
`SUNWcs@0.5.11-2026.0.0.23290` and similar. The oirepo6 Hipster 2026.04 zone
is pinned at 23029 by `osnet-incorporation`, so `pkg install -n -g` of the
RC11 `.p5p` failed. The RC11 binaries themselves load there: `ldd` resolves
every library and `config-test` succeeds.

## Decision

- `release-lib.sh` names the oldest supported OS level in
  `IPS_BASELINE_PINS`, one `STEM@VERSION` per line, as the oirepo6 zone has
  them installed: `consolidation/osnet/osnet-incorporation` and `SUNWcs`,
  `system/library`, `system/library/math` at `0.5.11-2026.0.0.23029`;
  `consolidation/userland/userland-incorporation` at
  `0.5.11-2026.0.0.33502`; `shell/ksh93` 93.21.1.20120801-2026.0.0.23029,
  `compress/xz` 5.8.3-2026.0.0.0,
  `library/security/openssl` 1.0.2.21-2026.0.0.18 and
  `system/library/gcc-14-runtime` 14.3.0-2026.0.0.1. The list changes only by
  a reviewed commit, so each tag records the baseline it was built for.
- `build-ips-package.sh` requires `IPS_BASELINE_IMAGE`, the root of a pkg(5)
  image in which every pinned package is installed at exactly its version.
  It fails if the variable is unset, if the directory is missing or is not an
  image, or if any pinned package is missing or at another version, newer or
  older.
- The build runs `pkgdepend -R "$IPS_BASELINE_IMAGE" resolve -m`, and fails
  if any dependency is unresolved, or if any resolved minimum version differs
  from the version installed in the baseline, pinned or not. The dependency
  stems, types, variants and other metadata still come from `pkgdepend`, and
  no version is removed or rewritten.
- After archiving, `pkg -R "$IPS_BASELINE_IMAGE" install -nv --no-refresh
  -g <p5p> package/pkg6/depotd` must succeed before the archive reaches
  `dist`. The baseline is only read. (pkg5 spells the option `--no-refresh`.
  It has no `--no-refresh-catalogs`.)
- Binaries are still compiled on the build host. No illumos asset is
  uploaded until the target-native gate has passed on the oirepo6 zone:
  `pkg install -nv -g`, `ldd -r` and `config-test`. Its evidence goes in the
  release notes (`docs/release-process.md`, Target-native gate).
- The release-script self-tests exercise the build's baseline logic with fake
  pkg5 tools. The fakes reject unknown options and derive the resolved
  minimum versions from the fake image. They cannot prove that real pkg5
  behaves the same. Only the oibuild build against the real baseline and the
  target-native gate prove that.

## Alternatives rejected

- **An allowlist that maps dependency versions down to minimum versions.**
  It would have to be maintained by hand for every dependency, and it records
  versions that were never resolved against a real image. We keep it in
  reserve in case a baseline image stops being practical.
- **Stripping versions from `depend` actions.** It would allow installation
  on OS levels the package was never checked against.
- **Updating the target's incorporation to fit the package.** The target's
  OS level is the operator's decision, not the package's.
- **Compiling against a baseline sysroot.** This is not practical with the
  current `cargo build` on oibuild. The check on a baseline-level system
  covers the risk instead.

## Consequences

- The illumos build needs a one-time baseline image on the build host
  (`packaging/ips/README.md`, Dependency baseline). On oibuild it is
  `/ws/toasty/ips-baseline-2026.04`, a persistent path rather than
  `/var/tmp`. Status on 2026-09-24: `pkg list` there shows both
  incorporations and all seven dependency packages at exactly their pinned
  versions, and `pkgdepend -R` of that image `resolve -m` on a copy of
  RC11's generated `.dep` wrote the target's minima (`SUNWcs` and
  `shell/ksh93` at 23029, `compress/xz` 5.8.3, `openssl` .18,
  `gcc-14-runtime` .1 and so on). No release package has been built against
  it yet.
- The image is dependency-resolution metadata, not a runnable OS. Installing
  the OS packages into a non-root user image reports `add_drv` failures, but
  `pkg` exits 0 and the package metadata is installed, which is all
  `pkgdepend` and `pkg install -n` read.
- `userland-incorporation@0.5.11-2026.0.0.33502` is no longer in the public
  Hipster publisher. The baseline installs it from a baseline-only `.p5p`
  rebuilt from the target's installed manifest: same version and `depend`
  actions (9609), no files, a new packaging timestamp (SHA-256
  `827682772a3c81ceb737fb1ff0d45077f043b531828b387d23401b8e66b2a54d` for
  the oibuild one). It is not an official historical archive and must never
  be served to clients. `packaging/ips/README.md` describes how to make one.
- `v1.0.0-rc.11` stays as published. The fix ships with the next tag.
- If the oldest supported target moves, update `IPS_BASELINE_PINS` and
  rebuild the baseline image.
- A binary that uses a libc symbol version the baseline lacks passes the
  build. Only the check on a baseline-level system catches it.
