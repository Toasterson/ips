# Retention, snapshots and exclusions

Retirement, protected snapshots and import exclusions are on the v1
integration line and in the package. The reference is
`pkg6repo-retire(8)` for the commands and `pkg6-policy(5)` for the policy
file; both manual pages are installed with the package. Without a
`policy.kdl`, retirement is disabled and every imported version stays in
the catalog.

## Two stages

1. **Retirement** hides superseded versions from
   catalogs and search. Exact `manifest/0` and `file/0|1` requests keep
   working, so `pkg fix` and `pkg verify` on systems that already have a
   hidden version still work.
2. **Purge (not in v1)** deletes content. There is no automatic
   purge and no S3 storage.

Retirement is separate from pkg5 obsoletion (`pkg.obsolete`) and from the
`*-obsoleted` store commands.

## Rules

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
  `<repo>/retention/access/`, so it needs write access to that
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

## Policy file

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

## Running retirement against a served repository

The depot runs as `pkg6srv` and creates `<repo>/retention/access/` when
it starts serving a generation-managed repository. Run `pkg6repo retire`
and `pkg6repo import-pkg5` as the same user, as in
[Installing](install.md):

```sh
su pkg6srv -c '/usr/bin/pkg6repo retire plan -s /var/pkg6depotd/repo -o /var/pkg6depotd/plan.json'
su pkg6srv -c '/usr/bin/pkg6repo retire apply -s /var/pkg6depotd/repo /var/pkg6depotd/plan.json'
```

`retire apply` takes the access fence in `retention/access/fence.lock`
and creates the directory if the depot has not yet done so. Run as root,
it can leave root-owned files there that the depot cannot open. The depot
then logs that it cannot record accesses and keeps serving, but access
history is unproven and no version becomes an inactivity candidate until
the ownership is fixed (`chown -R pkg6srv:pkg6srv
/var/pkg6depotd/repo/retention`) and the depot is restarted.

The package does not deliver anything under `/var/pkg6depotd`: the
repository, its policy and its access records are operator data.

## Still open

- Stage two: purge, enforcing the purge grace, and payload reference
  accounting.
- Validation of snapshot closures with real 2020-era OpenIndiana ISO
  clients.
- Tooling to derive snapshot inventories from ISOs.
