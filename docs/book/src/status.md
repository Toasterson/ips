# Status and limitations

This page reflects the v1 integration line as of 2026-09-23. No v1 tag
has been cut.

## On the integration line

| Area | Chapter |
|------|---------|
| IPS package `package/pkg6/depotd`, SMF service, fail-closed configuration | [Installing](install.md), [Configuration](configuration.md) |
| Read-only pkg5 depot protocol; pkg5 obsoletion in the main catalog | [Serving](serving.md) |
| Atomic multi-publisher `.p5p` import, idempotent retries, conflict rejection, adoption | [Atomic p5p imports](importing.md) |
| Local import of historical pkg5 content | [Historical imports](historical-imports.md) |
| Direct TLS: certificate files, ACME http-01 and dns-01 (Cloudflare, Gandi) | [TLS and ACME](tls.md) |
| OTLP/HTTP traces, metrics and logs through bounded queues | [OpenTelemetry](telemetry.md) |
| Retirement, protected snapshots, import exclusions, depot access recording | [Retention](retention.md) |
| Tag-only release pipeline with provenance and immutable assets | [Release process](release.md) |

## Pending

- **Purge.** The second retirement stage is not implemented anywhere.
- **Operation telemetry.** `pkg6depotd.operations` and
  `pkg6depotd.operation.duration` are defined but not recorded.
- **Ingest into managed repositories.** `pkg6recv`, `restore-obsoleted`
  and transaction publishing are refused on generation-managed repositories. A
  generation-backed batch writer is post-v1.
- **Remote publication and OIDC/RBAC.** Deferred past v1. HTTP is
  read-only.
- **Manual pages.** `pkg6depotd(8)`, `pkg6repo(1)`, `pkg6repo-retire(8)`
  and `pkg6-policy(5)` are packaged. There are no pages for `pkg6recv` or
  `pkgtree`.
- **Packaging identity.** UID/GID 94 registration and publisher
  confirmation are needed before GA.

## Not verified against the real system

- The real pkg5 `pkg refresh`/`install`/`update` against the depot during
  an import and across a crash. The tests emulate pkg5's catalog
  validation.
- Real power loss on ZFS. The tests inject failures at fixed points.
- The live Cloudflare and Gandi APIs. The tests use mocks built from the
  providers' documentation. Open questions:
  - Gandi `PATCH` on an absent RRset;
  - removing the last value of a Gandi RRset;
  - Cloudflare TXT quoting in list results.

## Known limitations

- Each import rebuilds the changed publishers' catalogs in full, and pkg5
  clients then download the full catalog.
- A pkg5 refresh that spans a generation switch fails once and succeeds on
  retry.
- Manifests whose only payload digest is SHA-2 `pkg.content-hash` are
  rejected.
- Crash-left `.<hash>.import` payload temporaries are not cleaned up.
- ACME:
  - no ARI, EAB, revocation, key rollover or tls-alpn-01;
  - dns-01 waits a fixed propagation delay;
  - one instance per state directory.
- HTTP/2 is not offered.
