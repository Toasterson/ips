# Installing the package and service

## The package

The illumos release job builds `pkg://openindiana.org/package/pkg6/depotd`.
It is published as the release asset
`pkg6depotd-<version>-illumos-x86_64.p5p`.

| Delivered | Path |
|-----------|------|
| Depot daemon | `/usr/lib/pkg6depotd` (after pkg5's `/usr/lib/pkg.depotd`) |
| Local tools | `/usr/bin/pkg6repo`, `/usr/bin/pkg6recv`, `/usr/bin/pkgtree` |
| Configuration | `/etc/pkg6depotd/pkg6depotd.kdl`, `root:pkg6srv`, mode `0640`, `preserve=renamenew` |
| SMF manifest | `/lib/svc/manifest/application/pkg/pkg6depotd.xml` |
| SMF method | `/lib/svc/method/svc-pkg6depotd` |
| Manual page | `pkg6depotd(8)` |
| Service account | user and group `pkg6srv`, UID/GID 94 |

The package delivers **no repository directory**. When pkg(1) removes a
directory, it moves any unpackaged content into `/var/pkg/lost+found`. A
delivered `/var/pkg6depotd` would therefore take your repository with it
on uninstall. You create the directory yourself.

The IPS version encodes the release channel in the branch, because IPS
versions cannot carry SemVer pre-release tags:

| Tag | IPS version |
|-----|-------------|
| `v1.0.0-alpha.1` | `1.0.0,5.11-0.1.1` |
| `v1.0.0-beta.1` | `1.0.0,5.11-0.2.1` |
| `v1.0.0-rc.1` | `1.0.0,5.11-0.3.1` |
| `v1.0.0` | `1.0.0,5.11-1` |

So a stable release sorts after all of its pre-releases.

## Install and first start

```sh
# pkg install -g pkg6depotd-<version>-illumos-x86_64.p5p package/pkg6/depotd
# zfs create -o mountpoint=/var/pkg6depotd rpool/pkg6depotd    # or mkdir
# chown pkg6srv:pkg6srv /var/pkg6depotd
# chmod 0750 /var/pkg6depotd
# su pkg6srv -c '/usr/bin/pkg6repo create /var/pkg6depotd/repo'
# su pkg6srv -c '/usr/bin/pkg6repo add-publisher -s /var/pkg6depotd/repo openindiana.org'
# vi /etc/pkg6depotd/pkg6depotd.kdl
# /usr/lib/pkg6depotd --config /etc/pkg6depotd/pkg6depotd.kdl config-test
# svcadm enable pkg6depotd
# svcs -l pkg6depotd
# tail "$(svcs -L pkg6depotd)"
```

Give the repository its own ZFS dataset. That lets you snapshot it before
large imports and keeps it independent of boot environments.

Run all repository commands as `pkg6srv`, so that everything the depot
reads stays owned by the service user.

## The service

`svc:/application/pkg/pkg6depotd`:

- The `default` instance is delivered **disabled**.
- It runs in the foreground (`--no-daemon`, `startd/duration=child`) as
  `pkg6srv`, with the pkg5 depot privilege set
  `basic,!file_link_any,!proc_info,!proc_session,net_privaddr`. It can
  therefore bind ports 80 and 443.
- It depends on `svc:/system/filesystem/local` and
  `svc:/milestone/network`.
- `stop` is `:kill`. SIGTERM stops accepting connections, gives running
  requests up to 10 seconds, flushes telemetry, then exits.
- `refresh` is `:kill -HUP`. SIGHUP only re-reads TLS certificates.
  **Configuration changes need `svcadm restart`.**
- The package restarts the service when the binary or method changes,
  and disables it before removal.

Service properties are defined on the service, not on the instance:

| Property | Meaning |
|----------|---------|
| `config/file` | Configuration path. Default `/etc/pkg6depotd/pkg6depotd.kdl`. |
| `config/log_filter` | `RUST_LOG` filter for the SMF log, for example `pkg6depotd=debug`. Empty selects the built-in default. |

```sh
# svccfg -s pkg6depotd setprop config/log_filter = astring: debug
# svcadm refresh pkg6depotd:default
# svcadm restart pkg6depotd:default
```

`svccfg -s pkg6depotd:default setprop ...` fails with "No such property
group", because the instance has no `config` group of its own.

### Several depots on one host

Give each additional instance its own `config` group. It overrides the
service value for that instance only:

```sh
# svccfg -s pkg6depotd add second
# svccfg -s pkg6depotd:second addpg config application
# svccfg -s pkg6depotd:second setprop config/file = astring: /etc/pkg6depotd/second.kdl
# svcadm refresh pkg6depotd:second
# svcadm enable pkg6depotd:second
```

Each instance needs its own bind address. If it uses ACME, it also needs
its own state directory.

## Upgrades and removal

- **Configuration you edited:** kept. The new default is installed next
  to it as `pkg6depotd.kdl.new`; compare the two after `pkg update`.
- **Configuration you did not edit:** replaced by the new default.
- **Uninstall:** an edited configuration is saved under
  `/var/pkg/lost+found`. `/var/pkg6depotd` is not touched.
- **SMF properties** set with `svccfg` survive upgrades.

## Failure modes at start

`pkg6depotd` has no built-in defaults. Any of the following makes both
`start` and `config-test` fail:

- a missing or unreadable configuration;
- a configuration that does not parse;
- anything other than exactly one valid `bind` address;
- an invalid `telemetry` or `tls` block.

In every case the instance ends up in maintenance, but by two routes:

- **Missing or unreadable file:** the start method checks this itself and
  exits with `SMF_EXIT_ERR_CONFIG`, so SMF moves the instance to
  maintenance at once.
- **Any other error:** the start method has already handed over to the
  daemon, which reports the error and exits non-zero (status 1). SMF
  restarts it, and after repeated failures moves the instance to
  maintenance.

To see the error without waiting for SMF, run
`/usr/lib/pkg6depotd --config <file> config-test`. Fix the file, then run
`svcadm clear pkg6depotd`.
