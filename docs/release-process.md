# Release process

Each release tag runs `.forgejo/workflows/release.yml`. The pipeline is the
same for every kind of tag. The design is recorded in
[ADR-005](ai/decisions/005-release-pipeline-and-ips-packaging.md).

For now, CI builds only the Linux assets. The illumos release job is
suspended until its throwaway-VM package gate works again
([ADR-011](ai/decisions/011-manual-illumos-release-assets.md)). A maintainer
builds the illumos assets from the exact tag on oibuild and uploads them from
a trusted machine. See [illumos assets](#illumos-assets-built-by-hand).
Branch CI (`ci.yml`) is not affected and still runs everything.

| Tag | Forgejo release | IPS version of `package/pkg6/depotd` |
|-----|-----------------|--------------------------------------|
| `v1.0.0-alpha.1` | pre-release, published once both platforms' assets are present | `1.0.0,5.11-0.1.1` |
| `v1.0.0-beta.1` | pre-release, published once both platforms' assets are present | `1.0.0,5.11-0.2.1` |
| `v1.0.0-rc.1` | pre-release, published once both platforms' assets are present | `1.0.0,5.11-0.3.1` |
| `v1.0.0` | draft; a maintainer publishes it | `1.0.0,5.11-1` |

Any other `v*` tag fails the pipeline.

## Cutting a release

Before tagging, run the [pre-tag pkg5 gate](#pre-tag-pkg5-gate) on an
OpenIndiana build host. It does not replace the live-image package test,
which currently does not run at all (see
[illumos assets](#illumos-assets-built-by-hand)).

1. Set `version` in `[workspace.package]` of `Cargo.toml` to the version
   without the `v`, for example `1.0.0-rc.1`. Refresh `Cargo.lock` with
   `cargo check`. Commit the change.
2. Create an annotated tag on that commit and push it:
   `git tag -a v1.0.0-rc.1 -m 'pkg6 1.0.0-rc.1' && git push origin v1.0.0-rc.1`.
3. The `linux` job does the following:
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
   4. Packages the build.
   5. Uploads its own assets. The release is created as a draft.
4. Build and upload the [illumos assets by hand](#illumos-assets-built-by-hand).
   The upload writes the builder and gate statement into the draft body
   before it uploads anything. When the last platform's assets are present,
   a pre-release is published automatically, whichever upload comes last.
5. A stable release stays a draft. To publish it:
   1. Check that the `provenance-*.json` files name the expected commit.
   2. Check the `SHA256SUMS-*` files against the assets.
   3. Read the illumos gate statement in the release body. Do not publish
      while the live-image row says **NOT RUN**. Either run
      `test-ips-package.sh` against the released `.p5p` on a disposable
      OpenIndiana VM and update the row, or have the owner accept the gap
      explicitly in the release body.
   4. Publish the release in the Forgejo UI.

Assets are uploaded only after their own gates pass. If one platform fails,
the release stays a draft with that platform's assets missing.

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

illumos re-uploads fail closed once its `.p5p` is on the release, unless
you upload the same `dist` directory again. A rebuild publishes the same
FMRI, but the archive bytes still record when it was built, so they never
match. To rebuild:

1. Make sure the release is still a draft. Never touch a published release.
2. In Forgejo, delete that platform's assets from the draft, or delete the
   draft release itself. The tag stays.
3. Re-run the Linux job, or rebuild and upload the illumos assets by hand.
   If you deleted the whole draft, do both.

Never move a published tag. A moved tag fails verification.

## Credentials

In CI, the upload uses the repository secret `RELEASE_TOKEN`: a personal
access token with `write:repository`. If that secret is not set, it falls
back to the runner's `FORGEJO_TOKEN`. Do not map `FORGEJO_TOKEN` through
`secrets.*` in the workflow. Solstice injects that token itself, and mapping
it would override the token with an empty value.

On a maintainer's machine, set `RELEASE_AUTH=basic` and use HTTP Basic
auth from two environment variables, `RELEASE_BASIC_USER` and
`RELEASE_BASIC_PASSWORD`. With `RELEASE_AUTH=basic`, both must be
non-empty; if either is empty or unset, for example because an `op read`
failed, the script stops. It never falls back to `RELEASE_TOKEN` or
`FORGEJO_TOKEN`, even if one is set in your shell. Read the pair from
1Password into the uploader's environment for that one command only, as in
[Upload](#4-upload), so no secret is written to disk, shell history or a
netrc file. Without `RELEASE_AUTH` (the CI path) the pair still wins over a
token when either half is set, and one half alone is refused.

The script copies the pair into unexported shell variables and unsets the
environment names before it starts its first child process, so the
programs it runs (git, jq, curl and the rest) do not inherit them. It hands
them to curl as a config on stdin (`--config -`), written by the shell's
`printf` builtin; curl builds the `Authorization: Basic` header itself. The
script never puts the password or its base64 form on a command line, in a
file or in its output. Unsetting does not erase the script's own initial
environment: while it runs, the same user or root can still read the pair
there (`pargs -e` on illumos, `/proc/<pid>/environ` on Linux, `ps eww` on
some systems), as they can for the `op read` that produced it. Run the upload
only on a trusted single-user machine. With two-factor authentication,
Forgejo rejects a password over Basic auth; store an access token with
`write:repository` in the password field instead.

Never put release credentials on oibuild or any other shared host.

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
  IPS_BASELINE_IMAGE=/ws/toasty/ips-baseline-2026.04 \
  .forgejo/scripts/build-ips-package.sh dist
```

This writes only to `target/ips` and `dist`. `IPS_BASELINE_IMAGE` is
required. It names a user image at the oldest supported OS level, and the
package's dependencies are resolved against it rather than against the build
host. Create it once, as described in `packaging/ips/README.md`, Dependency
baseline. The build only reads it.

### Pre-tag pkg5 gate

Before tagging, check on an OpenIndiana build host (for example the oibuild
zone), as an ordinary user, that the native pkg5 client (`pkg(1)`,
`pkgrecv`) consumes what this checkout's `pkg6repo` and `pkg6depotd` serve:

```sh
candidate=v1.0.0-rc.11   # replace; must be "v" + Cargo.toml workspace version
cargo build --workspace --release --locked -j2
RELEASE_TAG_OVERRIDE="$candidate" RELEASE_ALLOW_UNTAGGED=1 \
  IPS_BASELINE_IMAGE=/ws/toasty/ips-baseline-2026.04 \
  .forgejo/scripts/build-ips-package.sh dist
.forgejo/scripts/test-pkg5-unprivileged.sh \
  "dist/pkg6depotd-${candidate#v}-illumos-x86_64.p5p"
```

Set `candidate` to the tag you are about to cut. It must equal `v` plus
`version` in `[workspace.package]` of `Cargo.toml` (step 1 of Cutting a
release). `v1.0.0-rc.11` is only an example.

The `.p5p` is a local dry-run package built from the current `HEAD`; the tag
does not exist yet, so its manifest records `HEAD` as
`info.repository-changeset`. It is not a release asset. Do
not substitute a previous release's `.p5p`: it contains the old binaries, not
the code you are about to tag. After the tag is pushed, the real illumos
assets are built from the tag as described in
[illumos assets](#illumos-assets-built-by-hand).

The script imports the `.p5p` and a dummy package into a scratch repository,
checks that a re-import changes nothing, serves the repository with
`pkg6depotd` on a loopback high port, and runs `pkg` (catalog, `info -r`,
install, verify, uninstall into a scratch user image) and `pkgrecv` (listing
and byte-level comparison) against it. It uses no sudo, SMF or live image,
and removes its `/var/tmp/pkg6-pkg5-unpriv.*` directory on exit.
`PKG6_BIN_DIR`, `PKG6_SMOKE_PORT`, `PKG6_SMOKE_TMPDIR` and
`PKG6_SMOKE_KEEP=1` are described in its header. Run it for every release
candidate. It is not part of `release.yml`.

It is a fast gate, not a substitute: it does not test the package itself
(SMF service, `pkg6srv`, upgrade, uninstall). That is the job of
`test-ips-package.sh`, which needs a disposable VM and currently runs
nowhere.

Do not run `test-ips-package.sh` on a shared host. It installs packages and
changes SMF state on the live system. It also points the running depot at a
scratch repository for the pkg5 client smoke (see `packaging/ips/README.md`,
Validation).

## illumos assets built by hand

The illumos release job is suspended
([ADR-011](ai/decisions/011-manual-illumos-release-assets.md)). For every
tag, alpha to stable, a maintainer builds the illumos assets from the exact
tag on oibuild, copies them to a trusted machine and uploads them from
there. oibuild is shared: it never holds release credentials.

The build runs the same gates as the suspended job, except one:

| Gate | Where |
|------|-------|
| tag == `HEAD` == tag on origin; every crate version equals the tag | oibuild |
| release script self-tests | oibuild |
| release build, `-j2` | oibuild |
| nextest inventory (`release_check_test_inventory`) | oibuild |
| `cargo nextest run --workspace --release --locked`, no filters | oibuild |
| IPS package build (`build-ips-package.sh`), dependencies resolved against the baseline image, dry-run install into it | oibuild |
| native pkg5 smoke (`test-pkg5-unprivileged.sh`), no sudo | oibuild |
| tarball, `SHA256SUMS-illumos-x86_64`, `provenance-illumos-x86_64.json` | oibuild |
| target-native gate: `pkg install -n -g`, `ldd -r` and `config-test`; required before upload | the oirepo6 zone, by hand, read-only |
| **live-image install, upgrade, uninstall and SMF (`test-ips-package.sh`)** | **not run** |

The live-image test changes the live system, so it must not run on oibuild.
Until the Solstice VM gate is repaired it does not run at all. Every
release body must say so, and a stable release must not be published with
that gap open (step 5 of [Cutting a release](#cutting-a-release)).

### 1. Build on oibuild

oibuild needs `cargo-nextest` and `jq` on `PATH`, besides the Rust
toolchain and `package/pkg`. It also needs the dependency baseline image
(`packaging/ips/README.md`, Dependency baseline), owned by the build user.
oibuild runs a newer OS than the oldest target, and a package whose
dependencies are resolved against oibuild itself cannot be installed on the
target; that happened with `v1.0.0-rc.11`. Use a fresh clone of the tag, as an ordinary
user, in a session that survives a disconnect (for example `screen` or
`tmux`). The build takes a long time at `-j2`. `-j2` keeps the build within the host's memory, and
illumos `ld` fails without a message when it runs out.

```sh
tag=v1.0.0-rc.12   # the tag the Linux job is building
git clone --depth=1 --branch "$tag" https://code.aopc.cloud/OpenIndiana/ips.git "ips-$tag"
cd "ips-$tag"
bash -euxo pipefail ../build-illumos.sh "$tag" 2>&1 | tee "../build-$tag.log"
```

where `../build-illumos.sh` holds:

```sh
tag="$1"
export GITHUB_REF="refs/tags/$tag" CARGO_BUILD_JOBS=2
export FORGE_BASE=https://code.aopc.cloud REPO_OWNER=OpenIndiana REPO_NAME=ips
export IPS_BASELINE_IMAGE=/ws/toasty/ips-baseline-2026.04
. .forgejo/scripts/release-lib.sh
release_init
release_fetch_tag
release_verify_commit
release_verify_cargo_version
.forgejo/scripts/tests/test-release-scripts.sh
cargo build --workspace --release --locked -j2
cargo nextest list --workspace --release --locked --message-format json > target/nextest-list.json
release_check_test_inventory target/nextest-list.json
cargo nextest run --workspace --release --locked
.forgejo/scripts/build-ips-package.sh dist
.forgejo/scripts/test-pkg5-unprivileged.sh "dist/$(release_p5p_name)"
.forgejo/scripts/package-tarball.sh illumos-x86_64 dist
. target/ips/fmri.env
IPS_FMRI="$IPS_FMRI" .forgejo/scripts/write-provenance.sh illumos-x86_64 dist \
  "dist/$(release_tarball_name illumos-x86_64)" "dist/$(release_p5p_name)" "dist/$(release_manifest_name)"
release_platform_assets illumos-x86_64   # the five files to copy
```

`bash -e` stops at the first failing command; the log shows which one. If
a link fails at `-j2` with no other error, run `cargo test --workspace
--release --locked --no-run -j1` once, then the script again; a `-j1`
failure is a real error. Do not filter or skip tests. Keep the log: the
release body needs the test count from it.

### 2. Copy to a trusted machine and check

On the trusted machine (bash, git, jq and curl), clone the same tag, pull
the five files from oibuild and check them against the tag:

```sh
tag=v1.0.0-rc.12
git clone --depth=1 --branch "$tag" https://code.aopc.cloud/OpenIndiana/ips.git "ips-$tag"
cd "ips-$tag" && mkdir dist
for f in $(GITHUB_REF="refs/tags/$tag" bash -c '. .forgejo/scripts/release-lib.sh && release_init && release_platform_assets illumos-x86_64'); do
  scp "oibuild:ips-$tag/dist/$f" dist/
done
(cd dist && shasum -a 256 -c SHA256SUMS-illumos-x86_64)   # sha256sum -c on Linux
jq -r '.source.tag, .source.commit' dist/provenance-illumos-x86_64.json
git rev-parse "$tag^{commit}"   # must equal the commit above
```

#### Target-native gate

Do not go on to the upload (step 4) until this gate has passed on a
system at the baseline level, the oirepo6 zone. It is read-only there.
The build's dry run into the baseline image shows that the package's
dependencies fit an image with the pinned versions. It cannot show that
the real target agrees, or that binaries linked on the newer oibuild run on
the older OS. The release-script self-tests use fake pkg5 tools and show
neither. Copy the `.p5p` to the zone and keep the output of every command
below as the gate evidence:

```sh
p5p=pkg6depotd-<version>-illumos-x86_64.p5p
pkg list -v consolidation/osnet/osnet-incorporation \
  consolidation/userland/userland-incorporation   # ...23029 and ...33502
pkg install -nv -g "$p5p" package/pkg6/depotd      # must succeed; changes nothing
```

Then take `pkg6depotd`, `pkg6repo` and `pkg6recv` from the release tarball
checked in step 2. The package stages the same `target/release` files.
Run `ldd -r` on each: no line may say `not found` or `symbol not found`. Then run `pkg6depotd --config <kdl> config-test` with
the packaged `pkg6depotd.kdl`; it must exit 0. Any failure stops the
release. Never update the zone's incorporations to make the package fit.

### 3. Write the gate statement

Write the builder and gate status to a file, for example
`illumos-notes.md`. Fill in every row from what you ran; never mark a gate
passed that you did not see pass. The uploader refuses illumos notes unless
at least one line names the live-image and SMF test and gives its result as
`NOT RUN` or `passed`, and it refuses notes whose lines give both. The table
row below does that; so does a sentence such as "Live-image IPS install,
upgrade, uninstall and SMF lifecycle validation: NOT RUN".

```markdown
### illumos-x86_64

Built by hand from tag `v1.0.0-rc.12` on oibuild (OpenIndiana, `uname -v`:
...), YYYY-MM-DD, by <maintainer>; uploaded from a trusted machine. The
Solstice illumos release job is suspended (ADR-011). The Linux assets were
built and tested by Solstice CI.

| Gate | Result |
|------|--------|
| tag, HEAD, origin and crate versions agree | passed |
| release script self-tests | passed |
| release build (`-j2`) | passed |
| nextest inventory | passed |
| `cargo nextest run --workspace --release --locked` | passed, N tests |
| IPS package build, baseline `osnet-incorporation@0.5.11-2026.0.0.23029`, `userland-incorporation@0.5.11-2026.0.0.33502` | passed |
| native pkg5 smoke (`test-pkg5-unprivileged.sh`) | passed |
| target-native gate on the oirepo6 zone (osnet ...23029, userland ...33502): `pkg install -nv -g` | passed |
| target-native gate: `ldd -r` of `pkg6depotd`, `pkg6repo`, `pkg6recv` | passed, nothing unresolved |
| target-native gate: `pkg6depotd config-test` | passed |
| live-image install/upgrade/uninstall/SMF (`test-ips-package.sh`) | **NOT RUN** |
```

### 4. Upload

Upload only when the notes show all three target-native gate rows as
passed, with the evidence kept. The uploader does not check these rows, so
the maintainer must. Run this in the checkout of the tag from step 2:

```sh
RELEASE_AUTH=basic \
RELEASE_BASIC_USER="$(op read 'op://<vault>/<forgejo release item>/username')" \
RELEASE_BASIC_PASSWORD="$(op read 'op://<vault>/<forgejo release item>/password')" \
GITHUB_REF="refs/tags/$tag" FORGE_BASE=https://code.aopc.cloud \
REPO_OWNER=OpenIndiana REPO_NAME=ips \
RELEASE_NOTES_FILE=illumos-notes.md \
  .forgejo/scripts/upload-release-assets.sh illumos-x86_64 dist
```

The assignments prefix the command, so they are passed only to the
uploader's environment; nothing is exported into your shell, and history
records only the `op://` references. The script unsets the pair before it
starts any child process (see [Credentials](#credentials) for what that
does not cover). Do not `export` them, write them to a file, or run with
`set -x` on. If either `op read` fails and yields an empty value,
`RELEASE_AUTH=basic` makes the script stop instead of falling back to a
token.

The script is the one CI uses, with the same checks: it verifies the tag
against origin again, creates the draft if the Linux job has not yet,
refuses any existing asset with different bytes, and never adds to a
published release. Before any network call it checks that the notes state
the live-image/SMF result ([step 3](#3-write-the-gate-statement)). After the
asset checks it sets the draft body to the generated text followed by the
notes, replacing whatever the draft said before (such as the "Built by
Solstice CI" line of a draft an older script created), and checks that the
draft took it, still before uploading anything. It refuses illumos assets
without `RELEASE_NOTES_FILE`. Only after that does it upload, and if the
Linux assets are already there, it publishes a pre-release. A stable
release stays a draft. Re-running with the same `dist` and notes changes
nothing.

#### Tags cut before this procedure (`v1.0.0-rc.11`)

A tag carries the scripts of its own commit. `v1.0.0-rc.11` predates this
procedure: its `upload-release-assets.sh` has no `RELEASE_AUTH`,
`RELEASE_BASIC_*` or `RELEASE_NOTES_FILE`, so running it from the tag
checkout would ignore the notes and fall back to a token. For that tag, run
the new script by absolute path from a checkout of the branch that carries
it (the `v1-integration` branch until it is merged), with the detached
`v1.0.0-rc.11` checkout as the current directory:

```sh
new=/path/to/ips-v1-integration   # checkout of the branch with RELEASE_AUTH
cd "ips-v1.0.0-rc.11"   # the step 2 checkout of the tag; HEAD is detached there
git -C "$new" status --short -- .forgejo/scripts   # must print nothing
git -C "$new" log -1 --oneline -- .forgejo/scripts/upload-release-assets.sh
RELEASE_AUTH=basic \
RELEASE_BASIC_USER="$(op read 'op://<vault>/<forgejo release item>/username')" \
RELEASE_BASIC_PASSWORD="$(op read 'op://<vault>/<forgejo release item>/password')" \
GITHUB_REF="refs/tags/v1.0.0-rc.11" FORGE_BASE=https://code.aopc.cloud \
REPO_OWNER=OpenIndiana REPO_NAME=ips \
RELEASE_NOTES_FILE=illumos-notes.md \
  "$new/.forgejo/scripts/upload-release-assets.sh" illumos-x86_64 dist
```

The script loads `release-lib.sh` from its own directory, so both come from
the new checkout. Every git command runs in the current directory, so the
tag is fetched, verified against `HEAD` and origin, and recorded in the
release body from the `v1.0.0-rc.11` checkout. `dist` and the notes are
read relative to it too. Before you run it, the two `git -C "$new"` lines
must show a clean script directory and the reviewed commit; never upload
with uncommitted or unreviewed scripts. For every tag whose commit
contains this procedure, use the tag's own script with the relative path
shown above.

Upload after the Linux job has passed, so the pre-release is published
complete. If the Linux job fails, the release stays a draft.
