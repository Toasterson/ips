# Serving pkg5 clients

`pkg6depotd` speaks the pkg5 depot protocol. Point a client at it like any
other origin:

```sh
# pkg set-publisher -O https://pkg.example.org openindiana.org
# pkg refresh openindiana.org
```

## What is served

`/versions/0/` advertises these operations:

| Operation | Versions |
|-----------|----------|
| `versions` | 0 |
| `info` | 0 |
| `catalog` | 1, 2 |
| `manifest` | 0, 1 |
| `file` | 0, 1 |
| `publisher` | 0, 1 |
| `search` | 0, 1 |

Routes:

| Path | Notes |
|------|-------|
| `/{pub}/catalog/1/{part}` | pkg5 v1 catalog parts (`catalog.attrs`, `catalog.base.C`, ...) |
| `/{pub}/catalog/2/catalog.attrs`, `/{pub}/catalog/2/{sha256}` | pkg6 SQLite catalog shards |
| `/{pub}/manifest/{0,1}/{fmri}` | Byte-exact stored manifest |
| `/{pub}/file/1/{digest}`, `/{pub}/file/{0,1}/{algo}/{digest}` | Payload as stored: a single gzip member |
| `/{pub}/info/0/{fmri}` | Package info |
| `/publisher/{0,1}`, `/{pub}/publisher/{0,1}` | p5i publisher information |
| `/search/{0,1}/...`, `/{pub}/search/{0,1}/...` | Unscoped search uses the default publisher, as in pkg5 |
| `/ui/` | Web UI for browsing packages |
| `/admin/health` | Liveness probe only |

`GET` and `HEAD` work on catalog, manifest and file routes.

## What is not served

The public router mirrors pkg5's read-only depot. These pkg5 operations are
not routed:

- `index` (remote index rebuild);
- `admin/0`;
- publication: `open`, `add`, `close`, `abandon`;
- `catalog/0`.

Mutating request shapes get 404 or 405. A test asserts that they return
those codes and change no repository files. Rebuild catalogs and indexes
locally with `pkg6repo rebuild` or `pkg6repo refresh`.

## Consistency during imports

Readers always see one complete repository generation. An
[import](importing.md) publishes all publishers' catalogs with a single
`rename(2)`. The depot notices the switch on its next request and reloads.
Publisher lists, the default publisher and search follow without a
restart.

Every new catalog gets a modification time strictly later, in whole
seconds, than any catalog before it. So a pkg5 client revalidating
`catalog.attrs` with `If-Modified-Since` never gets a false `304`.

A client refresh that straddles a switch can fetch `catalog.attrs` from
one generation and a part from the next. The client then fails the part's
SHA-1 check, rejects the batch and retries against the new catalog. The
pkg5 protocol names parts without a version, so no server layout can avoid
this one retry.

A catalog rebuild changes the catalog's `created` time. pkg5 clients
therefore download the full catalog after each import. Incremental update
logs are follow-up work.

## pkg5 obsoletion

A package published with `pkg.obsolete=true` or `pkg.renamed=true` stays in
the main catalog, as in pkg5. The flags are written to
`catalog.dependency.C`, which is where the pkg5 solver reads them. In the
pkg6 SQLite shards, obsolete versions go to `obsolete.db` only, not
`active.db` or `fts.db`.

Repositories whose catalogs were built before this placement need one
`pkg6repo rebuild`. Until then, legacy clients do not see the flags.

The `*-obsoleted` commands of `pkg6repo` manage a separate, pkg6-only
inspection store. They are not pkg5 obsoletion and not retirement.
`pkg6repo obsolete-package` was removed.

## Not verified

The catalog-across-a-switch behaviour is covered by tests that emulate
pkg5's catalog validation. Running the real OpenIndiana `pkg refresh`,
`pkg install` and `pkg update` against the depot during an import is still
pending.
