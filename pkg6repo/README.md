# pkg6repo

pkg6repo is a Rust implementation of the Image Packaging System (IPS) repository management utility. It is designed to replace the pkgrepo command from the original IPS implementation.

## Installation

To build and install pkg6repo, you need to have Rust and Cargo installed. Then, you can build the project using:

```bash
cargo build --release
```

The binary will be available at `target/release/pkg6repo`.

## Usage

pkg6repo manages **local** repositories only. Every `-s`/`-d` argument is a
filesystem path; URIs (including `file://`) are rejected. The depot serves
repositories read-only over HTTP, and there is no remote administration in
v1, so there are no `--key`/`--cert` options.

A command either does what it reports or fails with a non-zero exit status,
without changing the repository when the failure is detected up front
(unknown publishers, unsupported properties, existing output files).

The manual page is `pkg6repo.1` in this directory.

### Create a Repository

```bash
pkg6repo create /path/to/repository
pkg6repo create --repo-version 4 /path/to/repository
```

Version 4 is the only format. `create` fails if the directory already holds a
repository configuration (`pkg6.repository` or `pkg5.repository`); it never
overwrites one.

### Add and Remove Publishers

```bash
pkg6repo add-publisher -s /path/to/repository example.com other.org
pkg6repo remove-publisher -n -s /path/to/repository example.com   # dry run
pkg6repo remove-publisher -s /path/to/repository example.com
```

`add-publisher` fails if any named publisher already exists.
`remove-publisher` deletes the publisher's catalog and manifests and fails,
removing nothing, if any named publisher does not exist. `-n` prints what
would be removed.

### Get and Set Properties

```bash
pkg6repo get -s /path/to/repository
pkg6repo get -s /path/to/repository -F json publisher/prefix
pkg6repo set -s /path/to/repository publisher/prefix=example.com
```

Only properties that have an effect are supported:

| Property             | Meaning                         | Settable |
|----------------------|---------------------------------|----------|
| `publisher/prefix`   | Default publisher               | yes      |
| `repository/version` | Repository format version (`4`) | no       |

Any other property is rejected. pkg5 publisher properties (`set -p`) are not
supported. Values written by earlier pkg6 builds under other names had no
effect and are not shown.

### Display Repository Information

```bash
pkg6repo info -s /path/to/repository [-p publisher]... [-F table|json|tsv] [-H]
```

### List Packages

```bash
pkg6repo list -s /path/to/repository
pkg6repo list -s /path/to/repository -p example.com -p other.org 'web/*' 'library/*'
```

Patterns are globs (`*`, `?`) matched against the whole package name. All
`-p` values and patterns are honoured. Obsolete package versions are listed,
as with pkg5. If a pattern matches nothing, the matches for the other
patterns are printed and the command exits non-zero naming the pattern.

### Show Package Contents

```bash
pkg6repo contents -s /path/to/repository example/package
pkg6repo contents -s /path/to/repository -t file -t link example/package
```

Prints one line per file, directory, link, dependency and license. Patterns
are regular expressions matched against the package name (unlike `list`).
`-t` accepts `file`, `dir`, `link`, `depend` and `license`. There is no `-m`:
pkg5 prints the manifest with it, which this command cannot do yet. Unmatched
patterns fail as with `list`.

### Search

```bash
pkg6repo search -s /path/to/repository [-p publisher] [-n limit] query
```

Without `-p` the default publisher is searched (all publishers if the
repository has no default). `-p` may be given once.

### Rebuild or Refresh Metadata

```bash
pkg6repo rebuild -s /path/to/repository [-p publisher]
pkg6repo refresh -s /path/to/repository [-p publisher]
```

Both rebuild the catalog and the search index, which is part of the catalog,
from the manifests. `--no-index` is accepted for pkg5 scripts but has no
effect (a warning says so). `--no-catalog` is not supported. On a repository
managed by `import-pkg5` the rebuild is published as a new generation.

### Import a pkg5 Repository

```bash
pkg6repo import-pkg5 -s /path/to/repository.p5p -d /path/to/pkg6/repository
pkg6repo import-pkg5 -s /path/to/pkg5/repo -d /path/to/pkg6/repository -p example.com -p other.org
```

The import is atomic: all packages of all selected publishers (default: all
in the source) become visible in one generation switch, or none do. A missing
destination is created. Re-importing a committed archive changes nothing. An
exact FMRI already published with a different manifest rejects the batch.
Packages with `pkg.obsolete=true` are published in the catalog like any other,
as in pkg5. See ADR-007.

### Obsolete Packages

To obsolete a package, publish a new version whose manifest sets
`pkg.obsolete=true`, as with pkg5. The `*-obsoleted` commands only inspect the
legacy obsoleted store that earlier pkg6 builds filled. They are not
obsoletion and not retirement. See `doc/obsoleted_packages.md`.

### Commands Removed Before v1

| Command             | Why                                                                 | Instead                                             |
|---------------------|---------------------------------------------------------------------|-----------------------------------------------------|
| `obsolete-package`  | Hid a package without pkg5 obsoletion semantics (ADR-004)            | Publish a `pkg.obsolete=true` manifest              |
| `cleanup-obsoleted` | Deleted store entries permanently, with no dependency or snapshot checks | None in v1; purge is part of retirement (#39)  |
| `import-obsoleted`  | Created new store entries, which hide packages from pkg5 clients     | None                                                |
| `archive`           | Wrote a pkg6-only `.p6p`; unmatched filters produced an empty archive | `pkg6recv` between local repositories               |
| `import-archive`    | Duplicated `pkg6recv` and was not atomic                              | `pkg6recv -s file.p6p -d repo <FMRI>...`            |

`pkg6recv` does not pick the newest version. A name or pattern such as `'*'`
works only for packages that have a single version in the source. If a package
has several versions, `pkg6recv` fails with `pkg6recv::ambiguous_version` and
receives nothing; name each such package with its exact version, e.g.
`pkg://publisher/name@1.0,5.11-2024.0.0.0:20240101T000000Z`. The error lists
the available versions.

`pkg6recv` checks the requested packages before it writes anything, but not
the rest of the receive. With `-r`, dependencies are resolved only while
receiving, and a missing dependency, manifest or payload stops the run
partway. Packages received before the failure stay in the destination and
its catalog is not rebuilt; run `pkg6repo rebuild -s <repo>` afterwards, or
receive into a scratch repository first.

See ADR-009.

## License

This project is licensed under the same license as the original IPS implementation.