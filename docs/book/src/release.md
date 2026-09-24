# Release process

Releases are made **only by pushing a tag**. The Forgejo workflow
`.forgejo/workflows/release.yml` builds, tests, packages and uploads the
Linux assets. No branch push produces a release.

For now the illumos release job is suspended: its throwaway-VM package gate
is not reliable. A maintainer builds the illumos assets from the exact tag
on an OpenIndiana build host and uploads them with the same upload script,
together with a statement of which gates ran. The live-image install,
upgrade, uninstall and SMF test does not run, and the release body says so.
`docs/release-process.md` in the repository has the procedure.

## Accepted tags

| Tag | Forgejo release | IPS version |
|-----|-----------------|-------------|
| `vX.Y.Z-alpha.N` | pre-release, published automatically once both platforms' assets are present | `X.Y.Z,5.11-0.1.N` |
| `vX.Y.Z-beta.N` | pre-release, published automatically once both platforms' assets are present | `X.Y.Z,5.11-0.2.N` |
| `vX.Y.Z-rc.N` | pre-release, published automatically once both platforms' assets are present | `X.Y.Z,5.11-0.3.N` |
| `vX.Y.Z` | stays a draft; a maintainer publishes it | `X.Y.Z,5.11-1` |

Any other `v*` tag fails the first step.

## Cutting a release

1. Set `version` in `[workspace.package]` of `Cargo.toml`, without the
   `v`, for example `1.0.0-rc.1`. Run `cargo check` to refresh
   `Cargo.lock`, then commit.
2. Tag that commit with an annotated tag and push the tag:

   ```sh
   git tag -a v1.0.0-rc.1 -m 'pkg6 1.0.0-rc.1'
   git push origin v1.0.0-rc.1
   ```

3. The `linux` job does the following. A maintainer runs the same steps
   for illumos by hand, except the live-image package test:
   1. Proves that the tag, `HEAD`, the runner commit and the tag on
      origin name the same commit, and that every crate version equals
      the tag. A moved or deleted tag fails.
   2. Runs the release script self-tests and checks this book (the
      Linux only; see [The book check](#the-book-check)).
   3. Builds the release profile.
   4. Checks the test inventory, then runs
      `cargo nextest run --workspace --release --locked` with no filters.
   5. Packages the build. On illumos this includes the IPS package and the
      unprivileged pkg5 client smoke. The live-image install, upgrade and
      uninstall test does not run while the illumos job is suspended.
   6. Writes `SHA256SUMS-<platform>` and `provenance-<platform>.json`,
      then uploads. The illumos upload first writes the builder and gate
      statement into the draft release body.
4. **Pre-release:** leaves draft when the second platform's assets
   arrive. **Stable:** check the provenance and checksums, read the
   illumos gate statement, then publish the draft in the Forgejo UI. Do
   not publish a stable release while the live-image test is marked
   **NOT RUN**, unless it has been run or the owner accepts the gap in
   the release body.

A version-bump commit must come first: a tag on code with another version
fails verification. Never move a published tag.

## Assets

| Asset | Contents |
|-------|----------|
| `ips-<version>-linux-x86_64.tar.gz` | Reproducible tarball of `bin/pkg6depotd`, `bin/pkg6repo`, `bin/pkg6recv`, `bin/pkgtree`, `LICENSE`, `README.md` |
| `ips-<version>-illumos-x86_64.tar.gz` | The same, built on OpenIndiana |
| `pkg6depotd-<version>-illumos-x86_64.p5p` | Installable IPS archive |
| `pkg6depotd-<version>-illumos-x86_64.p5m` | Published, dependency-resolved manifest |
| `SHA256SUMS-<platform>` | Checksums |
| `provenance-<platform>.json` | Tag, commit, tree, `Cargo.lock` hash, toolchain, artifact hashes |

The tarballs hold the depot server and the operator tools, the same
binaries the IPS package delivers. The `pkg6` client is not part of v1:
its multi-publisher image catalog behaviour is not release ready. Its
version is still checked against the tag, and CI still builds and tests
it, but no release asset contains it.

```sh
$ sha256sum -c --ignore-missing SHA256SUMS-illumos-x86_64
$ jq .source provenance-illumos-x86_64.json
```

## Immutable assets and re-runs

The upload script never deletes or replaces an asset.

| Existing asset | Result |
|----------------|--------|
| Identical bytes | Skipped. |
| Different bytes | The upload fails. |
| Absent, release still a draft | Uploaded. |
| Absent, release already published | The upload fails. |

Linux re-runs are clean. An illumos rebuild fails to upload once its
`.p5p` is on the release, because the archive embeds its publication time.
To rebuild, delete that platform's assets from the **draft**, then re-run
the Linux job or rebuild and upload the illumos assets.

## The book check

Pull-request CI and the `linux` release job both run
`docs/book/check-book.sh`. The script needs only POSIX `sh` and `awk`, and
fails on any of the following:

- a `SUMMARY.md` entry without a file;
- a page missing from `SUMMARY.md`;
- a broken relative link or `#anchor`;
- a link that leaves `docs/book/src`;
- an mdBook `{{#include}}`-style directive.

Before it, both jobs run `docs/book/test-check-book.sh`. That self-test fails
if either workflow stops running the book check, if a workflow downloads or
installs `mdbook`, or if `check-book.sh` stops rejecting any of the failures
above.

If `mdbook` is on the runner's `PATH`, the job also runs
`mdbook build docs/book`. Otherwise it logs that only the source check ran.
To check locally:

```sh
$ docs/book/test-check-book.sh
$ docs/book/check-book.sh
$ mdbook build docs/book -d /tmp/pkg6-book    # if mdbook is installed
```

## Before GA

- Register UID/GID 94 for `pkg6srv` in oi-userland
  `doc/reserved_uids_and_gids.md`.
- Get maintainer confirmation of the `openindiana.org` publisher for
  `package/pkg6/depotd`.
