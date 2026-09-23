# ADR-008: Direct TLS and a Built-in ACME Client for pkg6depotd

**Date:** 2026-09-23
**Status:** Proposed
**Issues:** [#41](https://code.aopc.cloud/OpenIndiana/ips/issues/41)
**Plan:** [2026-09-23-v1-direct-tls-acme](../plans/2026-09-23-v1-direct-tls-acme.md)

> Numbered 006 because 004 is claimed by the parallel v1 atomic-import work
> and 005 by the release pipeline. Other parallel v1 slices may also claim
> 006; renumber on merge if they do.

## Context

The v1 depot is deployed behind the same reverse proxy as oirepo, which
terminates TLS. The service must also be able to serve HTTPS itself, for
hosts without a proxy. Operators want certificates from Let's Encrypt without
an external client, using http-01 or dns-01. For dns-01 they use Cloudflare and
Gandi.

`ServerConfig` already had `tls-cert` and `tls-key` fields, but nothing read
them. A configuration that set them served plain HTTP without any warning.

## Decision

**Modes.**
- **Reverse proxy.** No `tls` block: plain HTTP on `server.bind`, as before.
- **Direct TLS.** A top-level `tls` block holds exactly one source: `certificate` + `key` files, or an `acme` block.
  - The old `server.tls-cert`/`tls-key` keys are now rejected.

**Fail closed.** `tls::plan` turns the block into typed settings (`TlsPlan`, `AcmeSettings`, `ChallengePlan`, `DnsPlan`, `DomainName`). Both `config-test` and `start` run it. The following are errors:
- a missing, extra or conflicting source;
- a relative path;
- an invalid or duplicate domain name;
- a wildcard with http-01;
- a domain outside the dns-01 zone;
- `terms-agreed` not set to true;
- a non-https directory (http is allowed only for loopback, for the tests);
- an unknown provider or challenge;
- an out-of-range TTL, renewal window or timeout.

`config-test` also loads and checks certificate files. It checks the token file mode (no access for other users) and the state directory mode (0700, owned by the service user). `start` binds the HTTPS port only after it holds a valid certificate. It never falls back to plain HTTP.

**ACME client in-tree.** The RFC 8555 client is written directly on crates that were already in `Cargo.lock`:
- ring for P-256 keys and ES256 JWS;
- reqwest with rustls;
- base64 and serde_json;
- a small DER module for the PKCS#10 CSR and for reading certificate validity and SANs.

Only `ring`, `base64 0.22` and `arc-swap` became direct dependencies of pkg6depotd. The lockfile gained three dependency lines and no new packages. Keeping the client in-tree means no new transitive dependencies, which matters for the illumos build and for review. It also means every loop has an explicit bound:
- stale-nonce retries: 3;
- polls: 60 per step, each delay capped at 30 s;
- response size: 1 MiB;
- order state changes: 8;
- requests: 30 s timeout;
- startup attempts: 3 per process start;
- issuance backoff: persisted, 10 s between the first attempts, then 5 min doubling, capped at 6 h (shared by startup and renewal, survives restarts);
- CA URLs: HTTPS only, or plain HTTP on the loopback origin of a plain-HTTP loopback directory (tests);
- order and authorization identifiers: exactly the requested names.

**State.** The state directory (0700) holds files written with a temporary file, fsync and rename (mode 0600):
- `account-<sha256(directory)[..8]>.json`: the account key and URL, one per directory. Staging and production never share an account.
- `order.json`: the order URL and certificate key. An interrupted order is resumed after a restart instead of being opened again. Orders that failed at the CA are discarded; orders interrupted by a network failure are kept.
- `certificate.pem`: the key and chain in one file, so a single rename replaces both and a crash can never pair a new chain with an old key.
- `challenges.json`: the dns-01 cleanup journal. Records are written here before the provider is called, updated with provider ids, and dropped once removed. A later start or issuance removes what an interrupted run left. We chose the journal over cleanup on SIGTERM: a stop during issuance (before the listener and its SIGTERM handler exist), a crash or SIGKILL cannot run cleanup, and the journal covers all of them.
- `backoff.json`: consecutive failed attempts and the earliest next attempt, keyed by directory and names, so SMF restarts cannot hammer the CA. A changed configuration starts afresh.

**Serving.** A rustls `ResolvesServerCert` reads the current `CertifiedKey` from an `ArcSwap`. It uses the ring provider explicitly, because both providers are compiled into the workspace.
- A new certificate is used by new handshakes immediately; existing connections are not dropped.
- A certificate is installed only if it parses, its key matches (`keys_match`, where "unknown" counts as a failure) and the current time lies within notBefore..notAfter. Startup, `config-test` and reload apply the same rule.
- Key files (operator key, ACME bundle) must not be accessible to other users.
- SIGHUP (SMF `refresh`, `:kill -HUP`) re-reads files or the ACME bundle. A failure keeps the old certificate. In plain-HTTP mode SIGHUP is only logged, so `refresh` cannot kill the depot.
- Handshakes run in separate tasks with a 10 s default timeout and at most 256 in flight. They feed axum's `Listener` trait, so the HTTPS path uses the same `axum::serve` as plain HTTP.
- ALPN offers only `http/1.1`, because axum is built without http2.
- SIGTERM/SIGINT drain HTTPS exactly like plain HTTP: stop accepting, give running requests the ADR-006 drain window, then flush telemetry. Both listeners share one graceful-shutdown path.

**Renewal.** The renewal point is `not_after - min(renew-before, lifetime/3)`. The lifetime rule keeps short-lived certificates renewed in time. A certificate that does not cover the configured domains is renewed immediately. At startup, a stored unexpired certificate is served and renewal runs in the background. With none, the depot tries three times and exits non-zero. SMF restarts it; the persisted backoff then delays the next attempts, so the instance stays online without listening rather than cycling into maintenance. Failed renewals back off exponentially and log at error level. After the certificate has expired, they state that clients will reject connections.

**Challenges.**
- **http-01.** A separate plain listener (`http01-bind`) answers only known tokens. It redirects other requests for configured names to HTTPS on the `server.bind` port and returns 404 for anything else, so it is not an open redirect.
- **dns-01.**
  - Cloudflare v4: zone lookup, or `zone-id`; one TXT record per value, commented `pkg6depotd ACME dns-01 challenge`; delete by record ID. If a create answer is lost, or the id never reached the journal, the record is found by exact name, value and comment; nothing else is deleted.
  - Gandi LiveDNS v5: `PATCH .../records/{name}/TXT` with `add_rrset_values` and `remove_rrset_values`, values quoted, so other values of the RRset survive. An absent RRset is created with POST (a 409 race falls back to PATCH). Cleanup reads the RRset and removes only our values still present. Never whole-RRset PUT or DELETE.
  - Both use a Bearer token read from a file. It is wrapped in `Secret`, whose `Debug` output is redacted.
  - Records and tokens are removed after every attempt, also after failures.
  - Provider and CA messages are truncated, and URLs are stripped from transport errors.

## Consequences

- Operators get HTTPS without a proxy and without certbot. Proxy deployments are unchanged.
- The ACME client is ours to maintain. Only the ACME features the depot uses are implemented: no ARI, EAB, revocation, key rollover or tls-alpn-01.
- dns-01 waits a fixed propagation delay instead of querying DNS, because there is no resolver crate.
- The Cloudflare and Gandi request shapes are covered by mocks written from the providers' API documentation. They have not been exercised against the live services. Unverified live behaviour: Gandi PATCH on an absent RRset (we expect 404) and removing the last value of an RRset (the mock drops the RRset); Cloudflare TXT content quoting in list results (we accept both).
- dns-01 records of an interrupted run stay at the provider until the depot starts or issues again.
- One depot instance per state directory. Concurrent instances sharing one are not locked against each other.
