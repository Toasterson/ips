# v1 CLI surface cleanup, part 1 (ticket #40)

Status: Implemented (pkg6, pkg6depotd); pkg6repo retirement is separate work.

## Goal

No shipped command may report success without doing its job.

## Done

- `pkg6depotd`: removed `stop`, `status`, `reload`, `health`, `admin
  auth-check` (they printed "not yet implemented" and exited 0) and the unused
  `--pid-file`. Unknown commands are clap usage errors, exit 2. The daemonize
  placeholder is gone: the depot always runs in the foreground. `--no-daemon`
  is hidden and ignored so existing scripts keep working; the SMF method and
  the Dockerfile no longer pass it. `pkg6depotd(8)` documents the foreground
  model, signals and exit status. Tests: `pkg6depotd/tests/cli_surface.rs`,
  including one that runs the SMF method's exact argument list.
- `pkg6`: `exact-install`, `uninstall`, `update`, `info`, `search`, `verify`,
  `fix`, `history`, `contents` are hidden from help, accept any arguments and
  fail with `pkg6::unsupported`, exit 2, before the image is opened.
  `install -g` and `install --licenses` fail the same way instead of being
  ignored. `install -n` is the dry-run option as in pkg(1); `-d` stays as an
  alias. Errors are printed as miette reports. Tests: `pkg6/tests/cli_surface.rs`
  and unit tests in `pkg6/src/main.rs`.
- Review follow-up: `pkg6 list -o` and `list` with more than one pattern fail
  with `pkg6::unsupported` instead of printing a table of something else;
  `list -q` prints nothing and `list -v` prints full FMRIs.
  `set-publisher -M` without `-O` fails instead of dropping the mirrors.
  `pkg6 help <hidden command>` says it is not implemented; `install -C` help
  says install runs sequentially. The Dockerfile passes `start` through `CMD`
  so `docker run <image> config-test` works. ADR-005 is amended for the SMF
  method, and the `start` tests run without `--no-daemon`.

- False-success follow-up: `pkg6 list` exits 1 when nothing is listed, as in
  pkg(1) (`-q`: exit status only). `pkg6 publisher <name>` fails, exit 1, on
  an unknown name, even next to a known one. `unset-publisher` no longer
  re-downloads the remaining catalogs and ignores failures: it deletes the
  removed publisher's catalog and rebuilds the merged catalog offline
  (`Image::purge_publisher_catalog`); a failed rebuild is an error that says
  the publisher was removed and to run `pkg6 refresh`. `install -C` fails with
  `pkg6::unsupported` (no linked images). `install --accept` is enforced:
  without it, an install whose plan has a `must-accept=true` license fails
  with exit 6 (pkg(1)'s EXIT_LICENSE) before changing anything; `-n` only
  warns. `--no-index` and `--no-refresh` stay: install never updates an index
  or refreshes, so what they ask for always holds; help says so.

## Remaining

- `pkg6 help <hidden command>` exits 0 (the text says it is not implemented);
  exit 0 for help is conventional.
- `install --accept` enforcement is unit-tested on the plan only; no
  end-to-end test installs a must-accept package from a repository.
- The merged image catalog holds one publisher's packages: each publisher's
  shard build replaces the previous one (`sqlite_catalog::build_shards`).
  Pre-existing libips issue, not a CLI false success.
- `pkg6` has no manual page.
