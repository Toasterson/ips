# Release process

Each release tag runs `.forgejo/workflows/release.yml`. The pipeline is the
same for every kind of tag. The design is recorded in
[ADR-005](ai/decisions/005-release-pipeline-and-ips-packaging.md).

| Tag | Forgejo release | IPS version of `package/pkg6/depotd` |
|-----|-----------------|--------------------------------------|
| `v1.0.0-alpha.1` | pre-release, published once complete | `1.0.0,5.11-0.1.1` |
| `v1.0.0-beta.1` | pre-release, published once complete | `1.0.0,5.11-0.2.1` |
| `v1.0.0-rc.1` | pre-release, published once complete | `1.0.0,5.11-0.3.1` |
| `v1.0.0` | draft; a maintainer publishes it | `1.0.0,5.11-1` |

Any other `v*` tag fails the pipeline.

## Cutting a release

1. Set `version` in `[workspace.package]` of `Cargo.toml` to the version
   without the `v`, for example `1.0.0-rc.1`. Refresh `Cargo.lock` with
   `cargo check`. Commit the change.
2. Create an annotated tag on that commit and push it:
   `git tag -a v1.0.0-rc.1 -m 'pkg6 1.0.0-rc.1' && git push origin v1.0.0-rc.1`.
3. Both jobs, `linux` and `illumos`, run independently. Each job does the following:
   1. Verifies that the tag, `HEAD`, the runner commit and the tag on origin
      are the same commit, and that every crate version equals the tag. For
      an annotated tag the runner may report the tag object instead of the
      commit; that is accepted, anything else is not.
   2. Runs the script self-tests and builds the release profile. The
      `linux` job also checks the operator book in `docs/book`
      (`docs/book/test-check-book.sh` and `docs/book/check-book.sh`, plus
      `mdbook build` when the image has mdbook). PR CI runs the same book
      step.
   3. Checks the test inventory, then runs `cargo nextest run --workspace
      --release` with no filters. The run includes the pkg6repo e2e tests,
      which drive the release `pkg6repo` binary. The inventory check fails
      the job if a required suite is missing or shrinks, or if any test other
      than the opt-in OpenIndiana network test would not run. The required
      suites (`RELEASE_REQUIRED_SUITES` in `.forgejo/scripts/release-lib.sh`)
      include the atomic pkg5 importer (ADR-007), OTLP telemetry
      (ADR-006), direct TLS/ACME (ADR-008), the pkg6repo and depot command
      surfaces with the manual page sync test (ADR-009), and retirement,
      snapshots and access recording (ADR-010) suites.
   4. Packages the build. The illumos job also builds the IPS package and
      installs, upgrades and removes it on its throwaway VM.
   5. Uploads its own assets.
4. The release is created as a draft. When the second job has uploaded its
   assets, a pre-release is published automatically.
5. A stable release stays a draft. To publish it:
   1. Check that the `provenance-*.json` files name the expected commit.
   2. Check the `SHA256SUMS-*` files against the assets.
   3. Publish the release in the Forgejo UI.

A job uploads only after its own tests pass. If one platform fails, the
release stays a draft with that platform's assets missing.

## Assets

| Asset | Contents |
|-------|----------|
| `ips-<version>-linux-x86_64.tar.gz` | Reproducible tarball of `bin/pkg6depotd`, `bin/pkg6repo`, `bin/pkg6recv`, `bin/pkgtree`, `LICENSE` and `README.md` |
| `ips-<version>-illumos-x86_64.tar.gz` | The same, built on OpenIndiana |
| `pkg6depotd-<version>-illumos-x86_64.p5p` | Installable IPS archive. Its FMRI timestamp is the tagged commit's commit time (UTC); the build fails unless the archive carries exactly the FMRI recorded in provenance |
| `pkg6depotd-<version>-illumos-x86_64.p5m` | The published, dependency-resolved manifest |
| `SHA256SUMS-<platform>` | Checksums of that platform's payloads |
| `provenance-<platform>.json` | Canonical public repository URL (never the runner's remote, which may carry credentials), tag, tag object, commit, tree, `Cargo.lock` hash, OS, toolchain, artifact hashes |

The tarballs carry the v1 release set, `RELEASE_BINARIES` in
`.forgejo/scripts/release-lib.sh`: the depot server and the operator tools,
the same binaries the IPS package delivers. The `pkg6` client is not
released in v1; its multi-publisher image catalog behaviour is not release
ready. It stays in `RELEASE_CRATES`, so its Cargo version must still equal
the tag, and CI still builds and tests it.

Verify a download:

```sh
sha256sum -c --ignore-missing SHA256SUMS-illumos-x86_64
jq .source provenance-illumos-x86_64.json
```

## Assets are immutable; re-runs

CI never deletes or replaces an asset. Before uploading anything, a job
compares every asset of its platform that is already on the release:

| Existing asset | Result |
|----------------|--------|
| identical bytes | skipped |
| different bytes | job fails ("with different content"), nothing uploaded |
| cannot be downloaded for comparison | job fails ("could not be downloaded to compare"), nothing uploaded |
| absent, release still a draft | uploaded |
| absent, release already published | job fails |

A re-run also finishes a release whose assets are all present but which is
still a draft, for example because the final publish call failed.

Linux re-runs are clean. The tarball is reproducible, and
`SHA256SUMS-*` and `provenance-*.json` contain only facts about the tagged
source, toolchain and artifacts. Per-run details (host, runner request) go to
the job log.

illumos re-runs fail closed once its `.p5p` is on the release. A rebuild
publishes the same FMRI, but the archive bytes still record when it was
built, so they never match. To rebuild:

1. Make sure the release is still a draft. Never touch a published release.
2. In Forgejo, delete that platform's assets from the draft, or delete the
   draft release itself. The tag stays.
3. Re-run the failed platform job. If you deleted the whole draft, re-run
   both jobs.

Never move a published tag. A moved tag fails verification.

## Tokens

The upload uses the repository secret `RELEASE_TOKEN`: a personal access
token with `write:repository`. If that secret is not set, it falls back to the
runner's `FORGEJO_TOKEN`. Do not map `FORGEJO_TOKEN` through `secrets.*` in
the workflow. Solstice injects that token itself, and mapping it would
override the token with an empty value.

## Local checks

```sh
.forgejo/scripts/tests/test-release-scripts.sh      # offline; git + jq
cargo nextest run -p pkg6depotd --test packaged_config --test config_fail_closed
```

On an OpenIndiana development machine, you can build the package without a
tag:

```sh
cargo build --release --locked
RELEASE_TAG_OVERRIDE=v0.0.0-alpha.0 RELEASE_ALLOW_UNTAGGED=1 \
  .forgejo/scripts/build-ips-package.sh dist
```

This writes only to `target/ips` and `dist`. Do not run
`test-ips-package.sh` on a shared host. It installs packages and changes SMF
state on the live system. It also points the running depot at a scratch
repository for the pkg5 client smoke (see `packaging/ips/README.md`,
Validation).
