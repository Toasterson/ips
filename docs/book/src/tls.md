# TLS and ACME

The depot runs in one of three modes:

| Mode | Configuration | Listener |
|------|---------------|----------|
| Reverse proxy (default) | no `tls` block | plain HTTP on `server.bind`; keep it private to the proxy |
| Certificate files | `tls { certificate; key }` | HTTPS only on `server.bind` |
| ACME | `tls { acme { ... } }` | HTTPS only on `server.bind`, plus an http-01 listener if configured |

With a `tls` block:

- The depot offers TLS 1.2 and 1.3 with ALPN `http/1.1` only; there is no
  HTTP/2.
- The handshake timeout is `handshake-timeout`, 1 to 120 seconds,
  default 10. At most 256 handshakes run at once.
- The depot opens the HTTPS port only once it holds a valid certificate.
- **It never falls back to plain HTTP.** An inconsistent `tls` block is an
  error at `config-test` and `start`: no source, both sources, a relative
  path, bad domains, and similar mistakes.

To serve HTTPS directly, bind the public address, for example
`server { bind "0.0.0.0:443" }`. The service holds `net_privaddr`.

## Certificate files

```kdl
tls {
    certificate "/etc/pkg6depotd/tls/fullchain.pem"   // chain, leaf first
    key "/etc/pkg6depotd/tls/key.pem"
}
```

```sh
# chown root:pkg6srv /etc/pkg6depotd/tls/*.pem
# chmod 0640 /etc/pkg6depotd/tls/*.pem
```

The depot refuses a key file that other users can access, a key that does
not match the certificate, and a certificate that has expired or is not
valid yet.

After you install a renewed pair, run `svcadm refresh pkg6depotd`, which
sends SIGHUP. New handshakes use the new certificate, and open connections
are not dropped. If the new files fail a check, an error is logged and the
old certificate stays in service. In plain-HTTP mode, SIGHUP is only
logged.

## ACME

The depot has a built-in RFC 8555 client. Start against the Let's Encrypt
staging CA, and remove `staging` once issuance works.

```kdl
tls {
    acme {
        domains "pkg.example.org"
        contact "mailto:hostmaster@example.org"
        terms-agreed true
        staging true
        state-dir "/var/pkg6depotd/acme"
        challenge "http-01"
        http01-bind "0.0.0.0:80"
    }
}
```

| Key | Meaning |
|-----|---------|
| `domains` | Up to 100 names. Wildcards need dns-01. |
| `contact` | Optional `mailto:` addresses. |
| `terms-agreed` | Required, must be `true`. |
| `staging` | Use the Let's Encrypt staging directory. |
| `directory-url` | Another CA's directory; https only. |
| `state-dir` | Required. Owned by `pkg6srv`, mode `0700`. |
| `renew-before-days` | 1 to 60, default 30. Renewal happens at the latest when a third of the lifetime is left. |
| `challenge` | `http-01` or `dns-01`. |
| `http01-bind` | Plain listener for http-01, usually `0.0.0.0:80`. |

Create the state directory before the first start, and keep it in backups:

```sh
# mkdir -m 0700 /var/pkg6depotd/acme
# chown pkg6srv:pkg6srv /var/pkg6depotd/acme
```

It holds these files, all mode `0600` and replaced atomically:

| File | Contents |
|------|----------|
| `account-*.json` | One account key per CA directory. |
| `order.json` | An order in progress. It resumes after a restart. |
| `certificate.pem` | The key and the chain in one file. |
| `challenges.json` | dns-01 records that may still exist at the provider. |
| `backoff.json` | Failed attempts. |

**Never delete `challenges.json` without first removing the records it
lists at the provider.** If the file is damaged, the depot refuses to
issue rather than overwrite it.

### http-01

The CA fetches `http://<domain>/.well-known/acme-challenge/<token>`. Port
80 of every domain must reach `http01-bind`. That listener answers known
tokens only. It redirects other requests for the configured names to HTTPS
on the `server.bind` port, and returns 404 for anything else. It is not an
open redirect.

### dns-01 with Cloudflare or Gandi

```kdl
tls {
    acme {
        domains "pkg.example.org" "*.pkg.example.org"
        contact "mailto:hostmaster@example.org"
        terms-agreed true
        state-dir "/var/pkg6depotd/acme"
        challenge "dns-01"
        dns01 {
            provider "cloudflare"          // or "gandi"
            api-token-file "/etc/pkg6depotd/dns-api-token"
            zone "example.org"
            // zone-id "..."               // Cloudflare only: skip the lookup
            // propagation-seconds 60      // 0 to 3600
            // ttl 120                     // Cloudflare 60..86400 (120); Gandi 300..2592000 (300)
        }
    }
}
```

The token file holds only the token. The token never appears in the
configuration, in `config-test` output or in logs.

```sh
# (umask 077; printf '%s\n' "$TOKEN" > /etc/pkg6depotd/dns-api-token)
# chown root:pkg6srv /etc/pkg6depotd/dns-api-token
# chmod 0640 /etc/pkg6depotd/dns-api-token
```

| Provider | Token | Record handling |
|----------|-------|-----------------|
| Cloudflare (API v4) | API token with Zone.DNS edit on the zone | One TXT record per value, with the comment `pkg6depotd ACME dns-01 challenge`. Deleted by id, or by exact name, value and comment. Nothing else is deleted. |
| Gandi LiveDNS (v5) | Personal access token with LiveDNS rights | Adds and removes only its own values in the `_acme-challenge` TXT RRset. Never replaces or deletes the whole RRset, so other values survive. |

Records are written to `challenges.json` before the provider is called.
They are removed after every attempt, including failed ones. Records left
by a crash are removed at the next start or issuance. The depot waits a
fixed `propagation-seconds`; it does not query DNS.

**Not verified:** both providers are tested only against mocks written from
their API documentation. Neither has been exercised against the live
service.

## Issuance, renewal and failure

- **At start with a valid stored certificate:** the depot serves it, and
  renewal runs in the background.
- **At start without one:** up to three attempts, 10 seconds apart, then a
  non-zero exit. SMF restarts the depot. The persisted backoff (5 minutes,
  then 10, 20, and so on, capped at 6 hours) then delays the next attempt.
  Meanwhile the instance is online but not listening. Delete
  `backoff.json` to retry at once. Changing the domains or the CA also
  resets it.
- **Renewal:** runs in the background, and the new certificate is used by
  new handshakes without a restart. A failed renewal logs at error level
  and backs off, and the current certificate stays in service.
- **`config-test`:** makes no ACME or DNS requests.

Not implemented: ARI, external account binding, revocation, key rollover
and tls-alpn-01. Run one depot instance per state directory; instances
sharing one are not locked against each other.
