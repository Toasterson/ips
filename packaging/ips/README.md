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
| `pkglintrc` | (build only) | pkglint gate: errors fail the build |

Binaries: `/usr/lib/pkg6depotd`, `/usr/bin/pkg6repo`, `/usr/bin/pkg6recv`,
`/usr/bin/pkgtree`. The service account is `pkg6srv` with UID/GID 94.

The package does not deliver the repository location. On uninstall, pkg(1)
moves unpackaged content of a directory it removes into
`/var/pkg/lost+found`, so a delivered `/var/pkg6depotd` would take the
repository with it. The operator creates `/var/pkg6depotd`, ideally as its
own ZFS dataset, and uninstalling the package leaves it untouched.

## Operator workflow

```sh
pkg install -g pkg6depotd-<version>-illumos-x86_64.p5p package/pkg6/depotd
zfs create -o mountpoint=/var/pkg6depotd rpool/pkg6depotd   # or mkdir
chown pkg6srv:pkg6srv /var/pkg6depotd && chmod 0750 /var/pkg6depotd
su pkg6srv -c '/usr/bin/pkg6repo create /var/pkg6depotd/repo'
su pkg6srv -c '/usr/bin/pkg6repo add-publisher -s /var/pkg6depotd/repo openindiana.org'
vi /etc/pkg6depotd/pkg6depotd.kdl     # bind address, repository root
/usr/lib/pkg6depotd --config /etc/pkg6depotd/pkg6depotd.kdl config-test
svcadm enable pkg6depotd
svcs -l pkg6depotd; tail "$(svcs -L pkg6depotd)"
```

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
  with each other.
- Offline: `cargo nextest run -p pkg6depotd --test packaged_config --test config_fail_closed`.
  These check that the default configuration parses with the daemon's schema,
  and that a missing or invalid configuration makes both `start` and
  `config-test` fail.
- On illumos, the SMF manifest validates against the illumos DTD
  (`svccfg validate`). CI runs the build and then `test-ips-package.sh` on a
  throwaway VM:
  - `pkgdepend generate` adds `org.opensolaris.smf.fmri` from the SMF
    manifest. The template must not set it: pkglint rejects duplicates with
    `manifest006`. The build checks that exactly one copy is generated.
  - `pkgrepo verify --disable dependency` checks the integrity of the
    one-package scratch repository. Its OS dependencies (SUNWcs, shells,
    system libraries) cannot resolve there. The live install in
    `test-ips-package.sh` is what checks that dependencies resolve.
  - Unexpanded `$(NAME)` macros abort the build only in action lines;
    comments are ignored.
