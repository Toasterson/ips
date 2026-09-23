# ADR-009: v1 pkg6repo Command Surface

**Date:** 2026-09-23
**Status:** Accepted
**Amends:** [ADR-004](004-v1-read-only-public-http-and-obsoletion.md) (decision 2), [ADR-007](007-atomic-pkg5-import-via-repository-generations.md) (`import-archive` is removed; `pkg6recv` is the remaining receive path)
**Issues:** #40, #24, #39

## Context

#40 asks for an audit of every shipped `pkg6repo` command and option before
v1, checked against the implementation rather than the help text. Local
repository paths are the only v1 target (ADR-004). The audit found these
commands and options that reported success without doing what they said,
or that destroyed data without the checks their names implied:

- **Remote options did nothing.** `-s` took a "path or URI", but a URI was
  opened as a relative directory (`create http://x` made a directory named
  `http:`). `--key`/`--cert` were accepted and ignored.
- **Filters were dropped.** `list`, `contents`, `rebuild`, `refresh` and
  `search` used only the first `-p` and the first pattern. `get -F json|tsv`
  ignored the section and publisher filters. `info`, `list` and `search`
  with an unknown `-p` printed nothing and exited 0. Unknown `-F` values
  were checked only after the repository was opened.
- **Invented output.** `contents -m` printed `file path=<p> type=<pkg id>`,
  which is not a manifest. `contents -t` accepted any action type and
  silently returned nothing for types the contents index does not record.
- **Rebuild flags.** `rebuild`/`refresh --no-catalog` did nothing at all
  (the search index is part of the catalog shards), and `--no-index` was
  ignored.
- **Inert properties.** `set` stored any `key=value`, and `set -p` stored
  publisher properties, in `pkg6.repository` `properties`. Nothing reads
  that map. `publisher/prefix`, which in pkg5 sets the default publisher,
  had no effect either. Values containing `=` were rejected.
- **Destructive surprises.** `create` on an existing repository rewrote
  its configuration with an empty publisher list. `remove-publisher` with an
  unknown name exited 0, and `-n` printed nothing. `add-publisher` with an
  existing name exited 0.
- **Obsoleted store.** `cleanup-obsoleted` permanently deleted store
  entries older than a TTL with no dependency, snapshot or retention check.
  `import-obsoleted` created new store entries. Store entries are not pkg5
  obsoletion and hide nothing from the catalog in a useful way (ADR-004).
  `export-obsoleted` with no matches exited 0 without writing a file. The
  read commands opened the store manager, which writes an index into
  `obsoleted/`. `search-obsoleted` help said "glob" while the pattern is a
  regular expression.
- **Archives.** `archive` wrote a pkg6-only `.p6p`. An FMRI filter that
  matched nothing produced an empty archive with exit 0, and the output file
  was overwritten. `import-archive` duplicated `pkg6recv` (same
  `PackageReceiver`, not atomic).

## Decision

The v1 command set is `create`, `add-publisher`, `remove-publisher`, `get`,
`set`, `info`, `list`, `contents`, `search`, `rebuild`, `refresh`,
`import-pkg5` and the store commands `list-obsoleted`, `show-obsoleted`,
`search-obsoleted`, `restore-obsoleted` and `export-obsoleted`. Repository
retirement gets its own verb, specified in #39 and outside this ADR.

1. **Local paths only.** Every repository and source argument goes through
   one value parser that rejects `scheme://` URIs, `file://` included, with
   a usage error (exit 2). `--key`/`--cert` are removed.
2. **Every filter is honoured or rejected.** `list`, `contents` and `info`
   accept repeated `-p` (and patterns for `list`/`contents`) and apply all
   of them. `rebuild`, `refresh` and `search` take `-p` at most once. An
   unknown publisher is an error. A pattern that matches nothing fails the
   command after the other matches are printed, as pkg5 does. `-F` and
   `contents -t` are enums that clap validates.
3. **`get`/`set` cover only effective properties:** `publisher/prefix`
   (the default publisher, settable to a configured publisher) and
   `repository/version` (read-only). Every other property and `-p` are
   rejected. All `set` arguments are validated before anything changes.
   Stored values from earlier builds stay in the file but are not shown.
4. **`contents -m` is removed** until it can print real manifests.
5. **`--no-catalog` is removed.** `--no-index` stays accepted for pkg5
   scripts, with a warning that it has no effect.
6. **Up-front checks for destructive commands.** `create` refuses a
   directory holding `pkg6.repository` or `pkg5.repository`.
   `add-publisher` refuses existing or repeated names, and
   `remove-publisher` refuses unknown ones, before changing anything.
   `remove-publisher -n` prints what it would remove.
7. **The store is legacy and read-mostly.** `cleanup-obsoleted` and
   `import-obsoleted` are removed. Read commands do not open the store
   manager on an unused store. `export-obsoleted` refuses an existing output
   file and fails, writing nothing, when no entry matches or every match
   lacks its metadata or manifest. libips writes the export to a temporary
   file in the target directory and links it into place without replacing
   anything, so a file created at the output path while the export runs is
   left untouched and the output path never holds a partial export.
   `restore-obsoleted` stays as the migration path: it publishes the stored
   manifest and removes the entry only after the commit. It is refused on
   generation-managed repositories.
8. **`archive` and `import-archive` are removed.** `pkg6recv` still reads
   `.p6p` archives and local repositories.
9. **Errors reach the user as miette diagnostics** (`main` returns
   `miette::Result`), not as `Debug` output.

## Consequences

- pkg5 scripts that pass `--no-catalog`, `--key`, `--cert`, `contents -m`,
  `set -p` or remote URIs now fail loudly instead of doing less than asked.
- Nothing in the v1 CLI can permanently delete obsoleted-store entries or
  create new ones. Purge arrives with retirement (#39), with its retention,
  snapshot and dependency checks.
- No v1 CLI produces `.p6p` archives. `ArchiveWriter` stays in libips for
  a later, deliberate archive design.
- `pkg6repo/tests/cli_surface.rs` drives the binary for each contract
  above. `pkg6repo/src/tests.rs` asserts the removed commands and options
  stay rejected at parse time.
- Still open, in libips (not changed here): `list` matches globs on the
  package name while `contents` matches regular expressions, falling back
  to a substring when the expression is invalid. `search` without `-p`
  searches only the default publisher. `refresh` is a full rebuild. See
  the #40 report.
