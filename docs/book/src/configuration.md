# Configuration

The depot reads one KDL file, `/etc/pkg6depotd/pkg6depotd.kdl` by default.
The packaged file is commented and is the best starting point. The
configuration can name TLS key and token files, so it is readable only by
root and the `pkg6srv` group.

## Minimal reverse-proxy configuration

```kdl
server {
    bind "127.0.0.1:8080"
}

repository {
    root "/var/pkg6depotd/repo"
    mode "readonly"
}

telemetry {
    service-name "pkg6depotd"
    log-format "json"
}
```

| Block | Key | Meaning |
|-------|-----|---------|
| `server` | `bind` | Exactly one `address:port`. With TLS, this is the HTTPS listener. |
| `repository` | `root` | Repository created with `pkg6repo create`. |
| `repository` | `mode` | `readonly`. HTTP never writes, whatever this says. |
| `telemetry` | `service-name`, `log-format` | Local logging and the OTel resource name. See [OpenTelemetry](telemetry.md). |
| `telemetry` | `otlp { ... }` | Optional OTLP export. See [OpenTelemetry](telemetry.md). |
| `tls` | `certificate`/`key` or `acme { ... }` | Optional direct HTTPS. See [TLS and ACME](tls.md). |

Rejected keys:

- **`server { tls-cert ...; tls-key ... }`** is rejected. Older
  configurations had it, but nothing ever read it. Use a `tls` block.
- **`telemetry { otlp-endpoint ... }`** is rejected. It never enabled
  export. Use `telemetry { otlp { endpoint ... } }`.

## Checking a configuration

```sh
/usr/lib/pkg6depotd --config /etc/pkg6depotd/pkg6depotd.kdl config-test
```

`config-test` prints only the file, the bind address, the repository root
and the TLS mode. It never prints the whole configuration, which can hold
credentials. Error messages give a line and column, not the offending
text.

It also checks the following, and stops there:

- With certificate files: loads the files and checks the key.
- With ACME: checks the permissions of the token file and the state
  directory.

It does not open the repository and makes no ACME or DNS requests.

Run `config-test` before every `svcadm restart`.
