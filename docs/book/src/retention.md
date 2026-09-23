# Retention, snapshots and exclusions (pending)

> **Pending.** Nothing in this chapter is on the v1 integration line or in
> the package. The design is accepted. An implementation is unmerged work
> in progress: it is not committed to the integration line, and has not
> been released or packaged. The `pkg6repo retire` commands and the `policy.kdl` file below
> do **not** exist in the installed tools. On the integration line every
> imported version stays in the catalog, and nothing is hidden or deleted.

This chapter describes the planned behaviour so that you can plan a
deployment around it. Details may still change until the work is integrated.

## Two stages

1. **Retirement (planned for v1)** hides superseded versions from
   catalogs and search. Exact `manifest/0` and `file/0|1` requests keep
   working, so `pkg fix` and `pkg verify` on systems that already have a
   hidden version still work.
2. **Purge (not planned for v1)** deletes content. There is no automatic
   purge and no S3 storage.

Retirement is separate from pkg5 obsoletion (`pkg.obsolete`) and from the
`*-obsoleted` store commands.

## Planned rules

- **Eligibility.** Only superseded versions can be retired: a newer
  version of the same package in the same publisher must exist, in pkg5
  order. The newest version is never retired, even when it is an obsolete
  or renamed marker.
- **Inactivity.** A policy candidate must be inactive for the configured
  interval, 12 months by default. The clock starts at the latest of
  supersession, last qualifying access, last restore, and the start of
  complete access history.
- **Qualifying access.** Access means a depot `GET` of an exact manifest,
  or of a payload that no other version of the publisher uses. Catalog,
  search, UI and `HEAD` requests do not count. An access resets the clock
  but does not unhide a version. Only an explicit local restore does.
- **Access records.** The depot appends records to
  `<repo>/retention/access/`, so it will need write access to that
  directory, though not to repository content. The history fails closed:
  - A crashed or stale depot, a failed write or a lost log line makes the
    history unproven, and no inactivity retirement happens.
  - `pkg6repo retire close-depot` records that a stale depot is gone.
  - A caching proxy in front of the depot hides accesses from it.
- **Plan, then apply.** `pkg6repo retire plan -s <repo> -o plan.json`
  writes a reviewable plan. `retire apply` takes the writer lock, rechecks
  every entry against the current generation and fresh access records,
  and publishes one new generation. Applying a plan twice is a no-op. A
  changed policy, an edited plan or another repository is refused.
- **Other verbs.** `retire restore`, `retire status` and
  `retire check-policy` complete the command set. Operators can also
  select versions explicitly with `--fmri`, as an exact FMRI or
  `regex:<pattern>`. That skips the inactivity test but no other check.

## Planned policy file

The policy is `<repo>/policy.kdl` (or `--policy`). Retirement is disabled
by default. Disabling it pauses retirement; it neither restores nor
purges.

```kdl
retirement {
    enabled true
    inactivity "12 months"
    purge-grace "12 months"   // validated and reported only
}
keep "pkg://openindiana.org/web/server/nginx@1.20,5.11-2021.0.0.1:20210601T101010Z"
keep regex="^pkg://openindiana.org/consolidation/.*"
keep-file "keep.txt"
import {
    exclude regex="^pkg://openindiana.org/developer/.*"
}
snapshot "hipster-2020.10" as-of="20201031T235959Z" {
    source iso="OI-hipster-gui-20201031.iso" sha256="..."
    fmri "pkg://openindiana.org/entire@2020.10,5.11-2020.0.1.0:20201031T010203Z"
}
```

- **`keep`** rules are exact FMRIs or regular expressions. They override
  both retirement and import exclusion.
- **`snapshot`** protects a historical set, such as an ISO's `entire`
  and incorporations, indefinitely. Its closure covers:
  - the members;
  - every version their `incorporate` dependencies match;
  - for other hard dependencies, the newest satisfying version published
    at or before `as-of`.

  The closure stays in the main catalog, so the legacy pkg5 solver sees
  it.
- **`import { exclude ... }`** leaves matching new packages out of an
  import. Excluded FMRIs are listed in the receipt.
- **Conflicts fail preflight, never silently.** Retiring or excluding a
  version fails before any change if the version is in a snapshot
  closure, or is the only satisfier of a hard dependency of a visible
  package. The error lists the exact `keep "<fmri>"` lines to add. Import
  conflicts use `ips::pkg5_import::exclusion_conflict`.

## Still open after integration

- Integration into the v1 line and packaging of its manual pages,
  `pkg6repo-retire(8)` and `pkg6-policy(5)`.
- Stage two: purge, enforcing the purge grace, and payload reference
  accounting.
- Validation of snapshot closures with real 2020-era OpenIndiana ISO
  clients.
- Tooling to derive snapshot inventories from ISOs.
