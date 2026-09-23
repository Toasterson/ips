# V1 Direct TLS and ACME for pkg6depotd

**Date:** 2026-09-23
**Status:** Active
**Issues:** [#41](https://code.aopc.cloud/OpenIndiana/ips/issues/41) (TLS/ACME slice)
**Decision:** [ADR-006](../decisions/006-direct-tls-and-acme.md)

## Goal

pkg6depotd runs behind the same TLS-terminating reverse proxy as oirepo, and
can also serve HTTPS directly. Direct TLS uses either certificate files or
certificates from an ACME CA, validated by http-01 or by dns-01 through
Cloudflare or Gandi. Everything fails closed, retries are bounded and logs
never contain secrets.

## Out of scope

- Live issuance against Let's Encrypt or live provider APIs (mock-tested only).
- ARI, EAB, revocation, account key rollover, tls-alpn-01, other DNS providers.
- HTTP/2, client certificates, several bind addresses.

## Steps

- [x] Config schema: a `tls` block with files or `acme` (`dns01` sub-block). The old `server.tls-cert`/`tls-key` keys are rejected.
- [x] `tls::plan`: typed validation shared by `config-test` and `start`.
- [x] `tls::der`: DER encoder and reader (CSR, validity, SANs, SPKI).
- [x] `tls::cert`: `LoadedCert` with a key-match check, a hot-swappable `CertStore` resolver, and reload.
- [x] `tls::listener`: TLS listener for `axum::serve`, with handshake timeout and concurrency cap.
- [x] `tls::acme`: RFC 8555 client with JWS and nonces. Account, order and certificate state are persisted with atomic writes. Covers resume, renewal loop, backoff and cleanup.
- [x] `tls::acme::dns`: Cloudflare and Gandi, with token files checked for permissions and a redacting `Secret`.
- [x] `tls::acme::http01`: challenge listener that redirects only the configured names.
- [x] `lib.rs`: TLS startup before the HTTPS bind, SIGHUP handler in both modes, and a TLS summary in `config-test`.
- [x] Packaging: commented TLS examples in the default config, SMF `refresh` method, TLS section in `pkg6depotd(8)`, README.
- [x] Tests:
  - unit tests (DER, CSR, JWS, thumbprint, state directory, token files, plan);
  - mock CA with JWS and nonce checks and real http-01 and dns-01 validation;
  - mock Cloudflare and Gandi APIs;
  - end-to-end tests that start the binary (files with SIGHUP reload, ACME at startup, refusal to start);
  - config fail-closed matrix;
  - packaged examples parse.
- [x] Review fixes: Gandi PATCH add/remove instead of whole-RRset PUT/DELETE; dns-01 cleanup journal (`challenges.json`) and Cloudflare recovery by name, value and comment; persisted issuance backoff (`backoff.json`); notBefore/notAfter and key file mode on startup, `config-test` and reload; CA URL policy; order/authorization identifier checks; http-01 redirect to the HTTPS port.
- [ ] Staging dry run on an OpenIndiana host with Let's Encrypt staging: http-01, then dns-01 with Cloudflare and with Gandi.
- [ ] Decide whether `test-ips-package.sh` should exercise `svcadm refresh` in plain mode.
