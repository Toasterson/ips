# IPS package: `package/pkg6/depotd`

These files build the OpenIndiana package of the pkg6 depot server and the
local operator tools. `.forgejo/scripts/build-ips-package.sh` builds the
package. The release process is described in
[docs/release-process.md](../../docs/release-process.md).

| File | Installed as | Notes |
|------|--------------|-------|
| `pkg6depotd.p5m` | package manifest | pkgmogrify template; macros come from the release tag |
| `pkg6depotd.xml` | `/lib/svc/manifest/application/pkg/pkg6depotd.xml` | `svc:/application/pkg/pkg6depotd`, default instance disabled |
| `svc-pkg6depotd` | `/lib/svc/method/svc-pkg6depotd` | start method; foreground daemon, `duration=child` |
| `pkg6depotd.kdl` | `/etc/pkg6depotd/pkg6depotd.kdl` | `root:pkg6srv 0640`, `preserve=renamenew` |
| `pkg6depotd.8` | `/usr/share/man/man8/pkg6depotd.8` | mdoc; check with `mandoc -T lint` |
| `../../pkg6repo/pkg6repo.1` | `/usr/share/man/man1/pkg6repo.1` | mdoc; repository commands |
| `../../man/man8/pkg6repo-retire.8` | `/usr/share/man/man8/pkg6repo-retire.8` | mdoc; retirement commands |
| `../../man/man5/pkg6-policy.5` | `/usr/share/man/man5/pkg6-policy.5` | mdoc; repository `policy.kdl` |
| `pkglintrc` | (build only) | pkglint gate: errors fail the build |

Binaries: `/usr/lib/pkg6depotd`, `/usr/bin/pkg6repo`, `/usr/bin/pkg6recv`.
The service account is `pkg6srv` with UID/GID 94. `pkgtree` ships only in
the release tarballs: OpenIndiana's `developer/pkgtree` already delivers
`/usr/bin/pkgtree`, and pkg(1) refuses to install two packages that deliver
the same file.

The package does not deliver the repository location. On uninstall, pkg(1)
moves unpackaged content of a directory it removes into
`/var/pkg/lost+found`, so a delivered `/var/pkg6depotd` would take the
repository with it. The operator creates `/var/pkg6depotd`, ideally as its
own ZFS dataset, and uninstalling the package leaves it untouched.

The same holds for everything written inside a repository: generations,
`policy.kdl`, retirement state and the depot's access records in
`<repo>/retention/access/`. The depot runs as `pkg6srv` and creates that
directory itself, so the repository must be owned by `pkg6srv`, and
`pkg6repo` commands against it (`import-pkg5`, `retire`) run as `pkg6srv`
too. A root-run `pkg6repo retire apply` can leave a root-owned
`retention/access/fence.lock` that stops the depot from recording
accesses; see pkg6repo-retire(8).

## Dependency baseline

`pkgdepend resolve` records the version of each dependency that is installed
in the image it resolves against as the minimum version. For RC11 that image
was the build host, which was newer than the target, so the package required
`SUNWcs@0.5.11-2026.0.0.23290`. The oirepo6 Hipster 2026.04 zone is pinned at
23029 by `osnet-incorporation`, so `pkg install` refused the package, even
though the binaries run there.

`build-ips-package.sh` therefore resolves against a baseline image, not the
build host. `IPS_BASELINE_PINS` in `.forgejo/scripts/release-lib.sh` lists
what the oirepo6 zone has installed, one `STEM@VERSION` per line:

| Package | Target version |
|---------|----------------|
| `consolidation/osnet/osnet-incorporation` | 0.5.11-2026.0.0.23029 |
| `consolidation/userland/userland-incorporation` | 0.5.11-2026.0.0.33502 |
| `SUNWcs`, `system/library`, `system/library/math` | 0.5.11-2026.0.0.23029 |
| `shell/ksh93` | 93.21.1.20120801-2026.0.0.23029 |
| `compress/xz` | 5.8.3-2026.0.0.0 |
| `library/security/openssl` | 1.0.2.21-2026.0.0.18 |
| `system/library/gcc-14-runtime` | 14.3.0-2026.0.0.1 |

`IPS_BASELINE_IMAGE` must name the root of a pkg(5) image that has every one
of them installed at exactly that version. The build fails if the variable
is unset, if the image is missing, if any pinned package is missing or at
another version, newer or older, or if a dependency does not resolve there.
After `pkgdepend resolve`, every versioned dependency must require exactly
the version the baseline has installed, pinned or not. The build then dry-runs
`pkg -R "$IPS_BASELINE_IMAGE" install -nv --no-refresh -g <p5p>` of the
finished archive, and fails if the baseline cannot install it. The baseline
is only read. Only the minimum versions depend on the baseline: the
dependency stems, dependency types and variants come from `pkgdepend` as
before, and no version is removed.

The baseline is dependency-resolution metadata for `pkgdepend` and `pkg
install -n`, not a runnable OS image. Create it once on the build host, as
the build user. It is a user image, so no root is needed.

The public Hipster publisher no longer carries
`userland-incorporation@0.5.11-2026.0.0.33502`, so installing it from there
fails. Bootstrap it from a baseline-only archive instead: a `.p5p` holding
that one package, rebuilt from the manifest installed on the target, with
its version and its `depend` actions (9609 of them) unchanged and no files.
Its packaging timestamp differs from the original's. It is not an official
historical archive. Keep it with the build host's tooling, and never add it
to a repository or serve it to clients. The archive used for
`/ws/toasty/ips-baseline-2026.04` has SHA-256
`827682772a3c81ceb737fb1ff0d45077f043b531828b387d23401b8e66b2a54d`; use
that one where you can. To make a new one from the manifest installed on a
system at the baseline level (the oirepo6 zone):

```sh
stem=consolidation/userland/userland-incorporation
pkg contents -m "$stem" | grep -E '^(set|depend) ' >baseline.p5m
pkgrepo create -s baseline-repo
pkgsend -s baseline-repo publish baseline.p5m
pkgrecv -s baseline-repo -a -d userland-incorporation-33502-baseline.p5p "$stem"
pkg list -g userland-incorporation-33502-baseline.p5p -afv "$stem"
```

`pkgsend` keeps the manifest's `pkg.fmri` version and gives it a new
timestamp. The last `list` must show `0.5.11-2026.0.0.33502`, and
`grep -c '^depend ' baseline.p5m` must count 9609. Then pin every package
explicitly. With the bare
names, a fresh image on current Hipster resolves the userland packages to
newer versions than the target has, and the build then fails its pin check:

```sh
base=/ws/toasty/ips-baseline-2026.04
p5p=/path/to/userland-incorporation-33502-baseline.p5p
pkg image-create -U -p openindiana.org=https://pkg.openindiana.org/hipster "$base"
pkg -R "$base" install --no-refresh \
  consolidation/osnet/osnet-incorporation@0.5.11-2026.0.0.23029 \
  SUNWcs@0.5.11-2026.0.0.23029 system/library@0.5.11-2026.0.0.23029 \
  system/library/math@0.5.11-2026.0.0.23029 \
  shell/ksh93@93.21.1.20120801-2026.0.0.23029 \
  compress/xz@5.8.3-2026.0.0.0 \
  library/security/openssl@1.0.2.21-2026.0.0.18 \
  system/library/gcc-14-runtime@14.3.0-2026.0.0.1
pkg -R "$base" install --no-refresh -g "$p5p" \
  consolidation/userland/userland-incorporation@0.5.11-2026.0.0.33502
pkg -R "$base" list -v consolidation/osnet/osnet-incorporation \
  consolidation/userland/userland-incorporation SUNWcs system/library \
  system/library/math shell/ksh93 compress/xz library/security/openssl \
  system/library/gcc-14-runtime
```

The `list` must show every pinned version from the table. In a user image
the first `install` reports `add_drv` failures, because driver actions need
root, but `pkg` exits 0 and the package metadata is installed. That is
enough for a resolution baseline; the image does not boot or run anything.
`image-create` refreshes the public catalog; the `install`s use its cached
contents and the local archive. The build user must own the image, because the build's
dry-run install takes the image lock. If a later `pkgdepend resolve` reports
a file no installed package delivers, install the package that delivers it
at the target's version (`pkg list -v` on the target), add it to
`IPS_BASELINE_PINS` in a reviewed commit, and build again.

Then build with the baseline:

```sh
IPS_BASELINE_IMAGE=/ws/toasty/ips-baseline-2026.04 .forgejo/scripts/build-ips-package.sh dist
```

The binaries are still compiled on the build host. A newer build host can
link against a library symbol version the baseline lacks, and the dependency
checks cannot see that. No upload may happen until the `.p5p` has been
checked on a system at the baseline level, the oirepo6 zone. There,
`pkg install -n -g <p5p> package/pkg6/depotd` must succeed, and so must
`ldd -r` and `config-test` of the extracted binaries. Record the evidence in
the release notes (`docs/release-process.md`, Target-native gate). Change
`IPS_BASELINE_PINS` only when the oldest supported target moves. Never
update the target's incorporations to make a package fit.

## Operator workflow

```sh
pkg install -g pkg6depotd-<version>-illumos-x86_64.p5p package/pkg6/depotd
zfs create -o mountpoint=/var/pkg6depotd rpool/pkg6depotd   # or mkdir
chown pkg6srv:pkg6srv /var/pkg6depotd && chmod 0750 /var/pkg6depotd
su pkg6srv -c '/usr/bin/pkg6repo create /var/pkg6depotd/repo'
su pkg6srv -c '/usr/bin/pkg6repo add-publisher -s /var/pkg6depotd/repo openindiana.org'
su pkg6srv -c '/usr/bin/pkg6repo import-pkg5 -s /var/tmp/userland.p5p -d /var/pkg6depotd/repo'
vi /etc/pkg6depotd/pkg6depotd.kdl     # bind address, repository root
/usr/lib/pkg6depotd --config /etc/pkg6depotd/pkg6depotd.kdl config-test
svcadm enable pkg6depotd
svcs -l pkg6depotd; tail "$(svcs -L pkg6depotd)"
```

## TLS

The default configuration serves plain HTTP on loopback for a reverse proxy
that terminates TLS, as in the oirepo deployment. To serve HTTPS directly,
bind the public address (`0.0.0.0:443`) and enable one of the commented
`tls` examples in `pkg6depotd.kdl`. The details are in pkg6depotd(8), under
TLS.

- **Certificate files:** put them in `/etc/pkg6depotd/tls/` with owner
  `root:pkg6srv` and mode `0640`; a key file other users can read is
  refused. After replacing them, run `svcadm refresh pkg6depotd`, which
  sends SIGHUP. New connections get the new certificate. If the new files
  are invalid, expired or not valid yet, an error is logged and the old
  certificate stays in service.
- **ACME:** start with `staging true`.
  - The state directory (for example `/var/pkg6depotd/acme`) is not
    delivered by the package, like the repository. Create it before the
    first start, owned by `pkg6srv` with mode 0700 (the daemon can also
    create it if `/var/pkg6depotd` is writable by `pkg6srv`). The daemon
    refuses a directory that other users can access or that another user
    owns. Keep it across upgrades and include it in backups: it holds the
    account key, the certificate key, and `challenges.json`. Never delete
    `challenges.json` without first removing the records it lists at the
    provider; a damaged one stops the depot rather than being overwritten.
  - For dns-01, write the provider token to a file with owner `root:pkg6srv`
    and mode `0640`. Name that file in `api-token-file`; the token itself
    never goes into the configuration.
  - The service already holds `net_privaddr`, so it can bind 443 and the
    http-01 listener on 80.

```sh
mkdir -m 0700 /var/pkg6depotd/acme && chown pkg6srv:pkg6srv /var/pkg6depotd/acme
(umask 077; printf '%s\n' "$CF_TOKEN" > /etc/pkg6depotd/dns-api-token)
chown root:pkg6srv /etc/pkg6depotd/dns-api-token && chmod 0640 /etc/pkg6depotd/dns-api-token
/usr/lib/pkg6depotd --config /etc/pkg6depotd/pkg6depotd.kdl config-test
svcadm restart pkg6depotd
```

If there is no certificate yet, the depot makes three issuance attempts and
exits. Failures are counted in `backoff.json` in the state directory, so
after SMF restarts it the depot waits 5 minutes, then 10, 20 and so on up to
six hours before contacting the CA again; the service looks online but does
not listen meanwhile. Delete `backoff.json` to retry at once. It never serves
plain HTTP instead. `config-test` makes no ACME requests.

dns-01 records that may still exist are listed in `challenges.json` before
they are created. If the depot stops during an issuance, they are removed at
its next start.

## Upgrades

The configuration file uses `preserve=renamenew`. After `pkg update`:

- If you edited the file, your copy stays in place. The new default is
  installed next to it as `pkg6depotd.kdl.new`; merge any changes you want.
- If you did not edit the file, it is replaced by the new default.
- If you uninstall the package, an edited file is saved under
  `/var/pkg/lost+found`. Repositories under `/var/pkg6depotd` are not
  touched.
- A missing or invalid configuration puts the service into maintenance;
  pkg6depotd has no built-in defaults to fall back to.

Service properties changed with `svccfg` also survive upgrades. The `config`
property group is defined on the service, so set it with
`svccfg -s pkg6depotd setprop config/...`, then run
`svcadm refresh pkg6depotd:default`. Setting it through `pkg6depotd:default`
fails with "No such property group". Additional instances override it with
their own `config` group; see pkg6depotd(8). The service
restarts when its binary or method changes. The package disables the service
before removing it.

## Validation

- Offline, on any platform: `.forgejo/scripts/tests/test-release-scripts.sh`.
  It checks that the manifest, SMF manifest, method and build script agree
  with each other, that the manifest and the build stage the same manual
  pages under the section their `.Dt` names (and that they pass
  `mandoc -Tlint` where mandoc is installed), and that no repository data
  (`var/`, `policy.kdl`, generations, `retention/`) is delivered.
- Offline: `cargo nextest run -p pkg6repo --test manpages`. Checks that
  `pkg6repo(1)` and `pkg6repo-retire(8)` document every subcommand and
  option of the built `pkg6repo`, and that cross references between the
  packaged pages resolve.
- Offline: `cargo nextest run -p pkg6depotd --test packaged_config --test config_fail_closed`.
  These check that the default configuration and its commented TLS examples
  parse with the daemon's schema, and that a missing or invalid configuration
  makes both `start` and `config-test` fail.
- Offline: `cargo nextest run -p pkg6depotd --test tls_config --test tls_acme --test tls_depot_e2e`.
  These check that TLS configuration errors stop the service, and run ACME
  issuance, renewal and reload against a mock CA and mock Cloudflare and
  Gandi APIs. They never contact a real CA.
- On illumos, the SMF manifest validates against the illumos DTD
  (`svccfg validate`). `test-ips-package.sh` runs after the build on a
  throwaway VM. It does not run at the moment: the illumos release job that
  ran it is suspended (ADR-011), and it must not run on a shared build host.
  The illumos release assets are built by hand and their release body marks
  this test **NOT RUN** (`docs/release-process.md`). When it runs, it checks:
  - `pkgdepend generate` adds `org.opensolaris.smf.fmri` from the SMF
    manifest. The template must not set it: pkglint rejects duplicates with
    `manifest006`. The build checks that exactly one copy is generated.
  - `pkgrepo verify --disable dependency` checks the integrity of the
    one-package scratch repository. Its OS dependencies (SUNWcs, shells,
    system libraries) cannot resolve there. The live install in
    `test-ips-package.sh` is what checks that dependencies resolve.
  - Dependencies resolve against `IPS_BASELINE_IMAGE`, and the archive must
    dry-run install there (see Dependency baseline). The release-script
    self-tests check the build script's logic with fake pkg5 tools. The
    fakes reject unknown options (so `--no-refresh-catalogs` fails) and
    derive the resolved minimum versions from the fake image. Each of these
    fails the build without leaving an archive in `dist`: a missing or
    non-image baseline; a newer, older or missing osnet or userland
    incorporation; a pinned dependency that is newer or missing; a resolved
    minimum above the baseline's version; an unresolved dependency; and a
    refused dry run. The fakes do not prove that real pkg5 accepts the
    commands, and they do not prove that a real image resolves or installs.
    Only the build on oibuild against the real baseline and the
    target-native gate show that.
  - Unexpanded `$(NAME)` macros abort the build only in action lines;
    comments are ignored.
  - The four manual pages are installed `0444 root:bin`, byte-identical to
    the staged pages, and render with `man -M /usr/share/man -s SECTION`.
    Uninstall removes them.
  - The pkg5 client smoke proves that the native OpenIndiana client consumes
    what `pkg6repo import-pkg5` publishes, served by the installed
    `pkg6depotd` under SMF as `pkg6srv`. It runs between the
    fail-closed configuration checks and the upgrade:
    1. The release `.p5p` and a dummy package without dependencies (built
       with `pkgsend` and `pkgrecv -a`) are imported with
       `/usr/bin/pkg6repo import-pkg5` into a scratch repository. The
       release archive is imported a second time. That must succeed, log
       `archive already imported`, and leave every path and file checksum
       of the repository unchanged (`pkg5_smoke_tree`).
    2. Negative control: while the depot still serves the operator
       repository, which lacks the release publisher, `pkg image-create`
       of a fresh image must succeed and its `pkg refresh --full` must then
       fail.
    3. The instance is switched to a copy of the operator config whose only
       change is the repository root, then restarted.
    4. A fresh scratch image (`pkg image-create -F --no-refresh`, then
       `pkg refresh --full`) must list exactly the source FMRIs, timestamps
       included. `pkg info -r` must fetch the release manifest.
    5. `pkgrecv -s <depot> --newest` must list the same FMRIs.
       `pkgrecv -a` pulls both packages into a scratch `.p5p`. Its
       manifests must be byte-identical to the sources. Its payloads must
       be gzip and decompress to the source bytes. It must hold no other
       manifest (`pkg5_smoke_compare` in `.forgejo/scripts/pkg5-smoke-lib.sh`).
    6. `pkg install` of the dummy into the scratch image must succeed. The
       installed file must match the proto, `pkg verify` must pass, and
       `pkg uninstall` must remove it.
    7. The instance is switched back to the operator config and restarted.
       It must no longer serve the release publisher.

    Without the depot's `/<pub>/catalog/1/` route, steps 4 and 5 fail. Without
    `/<pub>/manifest/0/`, steps 4 to 6 fail. Without `/<pub>/file/`, steps 5
    and 6 fail. A fresh image and a fresh `pkgrecv` cache mean nothing can
    come from a cache. `pkg(1)` (also under `sudo`, through `env`) and
    `pkgrecv` run with `no_proxy`/`NO_PROXY=127.0.0.1`, so a proxy on the
    VM cannot intercept the loopback depot. Everything lives in one `/var/tmp/pkg6-pkg5-smoke.*`
    directory, which is removed at the end or on failure.
    `/var/pkg6depotd` is never written, so the later upgrade and uninstall
    checks see the operator repository unchanged. The comparison and snapshot
    helpers are tested offline by `test-release-scripts.sh`. The smoke itself runs
    only inside `test-ips-package.sh`, which currently runs nowhere.
- Before tagging, as an ordinary user on an OpenIndiana build host, the
  pre-tag pkg5 gate runs the same client checks without root, SMF or the live
  image:

  ```sh
  candidate=v1.0.0-rc.11   # replace; must be "v" + Cargo.toml workspace version
  cargo build --workspace --release --locked -j2
  RELEASE_TAG_OVERRIDE="$candidate" RELEASE_ALLOW_UNTAGGED=1 \
    IPS_BASELINE_IMAGE=/ws/toasty/ips-baseline-2026.04 \
    .forgejo/scripts/build-ips-package.sh dist
  .forgejo/scripts/test-pkg5-unprivileged.sh \
    "dist/pkg6depotd-${candidate#v}-illumos-x86_64.p5p"
  ```

  `v1.0.0-rc.11` is only an example: set `candidate` to the tag you are about
  to cut, matching `version` in `[workspace.package]` of `Cargo.toml`. The
  `.p5p` is a local dry-run package built from the current `HEAD`, not a
  release asset, and a previous release's `.p5p` does not test the code about
  to be tagged. After tagging, the illumos release assets are built by hand
  from the tag (`docs/release-process.md`); the live-image test does not run.

  It imports the `.p5p` and a dummy owned by the running user with the
  checkout's `target/release/pkg6repo`, runs `target/release/pkg6depotd` in
  the foreground on a loopback high port, and uses a scratch user image
  (`pkg image-create -U`). Steps 1 and 4 to 6 above apply, in that user
  image; the negative control and the config switches do not, as there is
  no SMF instance. It is a fast gate before the tag, not a replacement for the
  live-image test above, which needs a disposable VM and does not run while the
  illumos release job is suspended. See `docs/release-process.md`, Pre-tag pkg5
  gate.
