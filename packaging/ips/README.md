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

Binaries: `/usr/lib/pkg6depotd`, `/usr/bin/pkg6repo`, `/usr/bin/pkg6recv`,
`/usr/bin/pkgtree`. The service account is `pkg6srv` with UID/GID 94.

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
