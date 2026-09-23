# ADR-010: Retirement, Protected Snapshots and Import Exclusions

**Date:** 2026-09-23
**Status:** Accepted
**Issues:** #39 (implementation), #40 (command semantics), #24
**Related:** [ADR-004](004-v1-read-only-public-http-and-obsoletion.md),
[ADR-007](007-atomic-pkg5-import-via-repository-generations.md),
[ADR-009](009-v1-pkg6repo-command-surface.md)

## Context

Maintainer decisions on #39/#40/#24 (2026-09-22):

- Two-stage retirement. Stage one hides superseded versions from catalogs
  and search; exact manifest and payload requests keep working. Stage two,
  purge, is a separate operator action (not in this release). No automatic
  purge, no S3.
- Retirement can be enabled and disabled; the inactivity interval (and the
  later purge grace) is configurable and defaults to 12 months. The policy
  file is KDL. Regex exclusions with exact-FMRI keep exceptions.
- A qualifying access to a hidden version resets its clock but leaves it
  hidden; only an explicit local restore makes it visible again.
- Selected historical snapshots stay installable and repairable under their
  own constraints, indefinitely, discoverable by the legacy pkg5 solver, not
  just by exact fetch.
- Import exclusions and retirement that conflict with the protected set fail
  preflight with explicit, exact keep entries for the operator to add. Never
  silently skip or override.
- pkg5 `pkg.obsolete` keeps pkg5 semantics (ADR-004).
- Public HTTP stays read-only; administration is local (ADR-004).

## Decision

1. **Hiding is a catalog property of a generation.** Each generation holds
   `retirement/state.json` (hidden versions with reason, time, operation,
   supersession and last access) and `retirement/audit/<operation>.json`.
   Both are hard-linked into every later generation, never rewritten in
   place. Catalog builds (`StagedGeneration::build_catalogs`) leave hidden
   versions out of the legacy v1 parts, and therefore out of `active.db`,
   `obsolete.db` and `fts.db`. Manifests stay in `pkg/`, payloads in `file/`:
   exact `manifest/0` and `file/0|1` requests are unchanged. Imports,
   rebuilds and publisher removal carry the state forward, so a hidden
   version stays hidden (re-importing its archive does not unhide it).
2. **Plan, then apply.** `pkg6repo retire plan` computes a plan against the
   published generation without the lock and writes it (JSON, with a
   content hash as its id, the base generation and the policy digest) for
   review. `retire apply` takes the writer lock and rechecks everything
   against the generation published *then* and fresh access records: every
   version must still exist, be superseded, not be kept, still be inactive
   (policy candidates), and hiding the set must pass the closure checks
   below. A changed policy, an edited plan or another repository is refused.
   On success one staged generation with the new state, rebuilt catalogs of
   the affected publishers and an audit record is published by the
   ADR-007 switch. Readers see the old catalogs until then.
3. **Idempotence.** A plan whose id is in the audit trail reports
   `AlreadyApplied` (this covers a crash after the switch). A plan whose
   versions are all hidden changes nothing. Restoring a visible version is a
   no-op; a selection matching no version at all is an error.
4. **Eligibility.** Only superseded versions (a newer version of the same
   package in the same publisher exists, pkg5 order: release, dash-branch,
   timestamp) are retired; the newest version, including a `pkg.obsolete`
   or `pkg.renamed` marker, never is. A policy candidate needs
   `now >= inactivity after max(superseded_at, last_access, restored_at,
   proven_since)` (decisions 11 and 15).
   `superseded_at` is the earliest publication of a newer version, where
   publication is the later of the FMRI timestamp and the first import
   receipt naming it. `proven_since` means no version is retired for
   inactivity before access has been recorded completely for a full
   interval.
   Intervals are calendar months (or days/weeks/years). Operator selections
   (`--fmri`, exact or `regex:`) skip the inactivity test, not the others,
   and work while policy-driven retirement is disabled.
5. **Qualifying access.** The depot records `GET` of an exact manifest and
   of a payload; catalog, search, info, UI and `HEAD` requests and our own
   scans do not. Records are appended, one line per version (or payload) and
   UTC day, to `<repo>/retention/access/<YYYYMMDD>.log` (manifest lines
   `fdatasync`ed), together with the control lines of decision 11. Apply
   folds logs older than yesterday into `summary.json` under the writer
   lock (recording the last folded day, so a crash before the logs are
   deleted never folds them twice). This lives outside the generations, is
   independent of telemetry, and is only kept on generation-managed
   repositories; a depot that cannot write it warns (rate-limited) and
   keeps serving, and retirement treats the history as incomplete. A payload access counts
   for a version only if no other version of its publisher references the
   payload, so shared payloads never refresh every referencing version.
6. **Protected snapshots.** A `snapshot` in the policy lists exact member
   FMRIs (typically an ISO's incorporations and packages) with provenance.
   Its closure is: the members; every version matching an `incorporate`
   dependency of a protected package (pkg5 `CONSTRAINT_AUTO`); and for each
   `require`, `require-any` alternative, `group` or `conditional` dependency
   not pinned by the snapshot's incorporations, the newest non-obsolete
   satisfying version published at or before the snapshot's `as-of` time
   (default: its newest member), recursively. Members the repository does
   not hold are reported as unavailable historical content. The closure
   stays in the main catalog, so the legacy solver sees every version at
   once; there is no separate snapshot catalog.
7. **Closure checks, blockers and suggestions.** Hiding a set is blocked if
   it hides a snapshot closure member, or makes a `require`, `require-any` or
   `incorporate` dependency of a package that stays visible unsatisfiable
   when it was satisfiable before (obsolete versions do not satisfy
   `require`). A snapshot version that is already hidden blocks every plan
   until it is restored. The error lists each blocker and the exact
   `keep "<fmri>"` entries (or restores) that resolve it. Keep rules
   override both retirement and import exclusion. Restoring a version whose
   hard dependencies only hidden versions satisfy is blocked the same way.
8. **Import exclusions.** `import { exclude ... }` rules leave matching new
   packages of an import out unless a `keep` rule matches. If an excluded
   package is in a snapshot closure or is the only satisfier of a hard
   dependency of a visible package, imported by this batch or already
   published, or whether it is needed cannot be checked (decision 14), the
   whole batch fails
   before anything is written (`ips::pkg5_import::exclusion_conflict`) with
   suggested keep entries. Excluded FMRIs are listed in the receipt and the
   command output; a retry of the same archive is a no-op only while the
   policy still excludes all of them.
9. **Policy file.** `<repo>/policy.kdl` (or `--policy`), with `retirement`,
   `keep`, `keep-file`, `import` and `snapshot` nodes; keep files may be
   text (`exact`, `regex:` lines), KDL or JSON. Unknown nodes, invalid
   regexes, non-exact "exact" FMRIs, zero or unknown durations fail before
   any mutation. Disabled by default; disabling pauses policy-driven
   retirement and neither restores nor purges. `purge-grace` is validated
   and reported only.
10. **Separate verb.** Retirement is `pkg6repo retire plan|apply|restore|
    status|check-policy`, distinct from pkg5 obsoletion and from the legacy
    `*-obsoleted` store commands. `pkg6repo` now renders errors as miette
    diagnostics so blockers and help reach the operator.

11. **Access history fails closed.** Each depot is a named recorder (host
    and listen address: stable across restarts, distinct between depots).
    It writes chained control lines to the day logs: `s` starts a run, `h`
    heartbeats every minute and as the first line of each day log it
    writes to, `e` ends a clean shutdown (SIGINT/SIGTERM); each carries the
    run's next sequence number and means "no access record of this run
    failed since my previous control line". A failed write ends the run on
    the spot; the recorder starts a new one once it can write. An `s` line
    chains to the recorder's previous run (its `<run>:<seq>`, `-` for a new
    recorder, `?` if the records were unreadable) and carries the earliest
    time from which the same process failed writes. Reading back, a new run
    after an unended one (crash or failed write), a new run whose chain
    does not match the recorder's last known control line (deleted logs,
    even ones holding only complete runs), a skipped sequence number
    (deleted or truncated log), a run started after failed writes (from the
    failure), a closed recorder writing again (from the close) or an
    unreadable line is a *gap*; history is proven from the later of the first run and the latest
    gap end. A run whose last line is older than ten minutes is *stale* (the
    depot may be alive and unable to write anything, e.g. a full disk, where
    no failure marker can be written): then nothing is proven. `pkg6repo
    retire close-depot` records an operator's statement that a stale depot
    is gone. Periods without any depot count as periods without access:
    pkg6depotd is assumed to be the only server of the repository.
    Each recorder also keeps a *head* (`retention/access/head.<hex name>`,
    its newest control line, replaced atomically after every control line;
    a failed update ends the run like a failed record). Logs that never
    reach a recorder's head (the newest logs deleted, even while every
    depot is stopped) are a gap up to the head, and the head stands in for
    the recorder's state, so a restart chains to it and the gap stays. A
    recorder with records but no head is a gap up to its last record; an
    unreadable head, or a summary folded through yesterday or later, is
    refused. Deleting heads together with logs is not detected.
12. **Final access check under a fence, synchronised with the switch.**
    `GenerationStore::commit_gated` runs a gate after the catalogs are built
    and the generation is durable, holding what it returns until the switch
    is done. For policy candidates, apply's gate takes an exclusive
    `flock(2)` of `retention/access/fence.lock`, publishes a nonce in
    `fence` and waits until every open run confirms it in a heartbeat
    (proving it failed no write so far), then rereads the records and
    checks every candidate again. Depots append access lines under a
    *non-blocking* shared lock: while apply holds the fence they queue
    records in memory and append them after the switch. No access recorded
    before the switch goes unseen, and HTTP requests never wait for apply
    (only for the `fdatasync` of a record they write).
13. **No clock override.** Eligibility, `hidden_at` and audit times use
    `retention::clock`, the wall clock. There is no command line option or
    API parameter for the time; unit tests and the test-only `test-clock`
    feature (enabled by libips' own dev-dependency on itself, never by the
    binaries) can pin it.
14. **Unverifiable content blocks.** A package version or dependency that
    is not a pkg5 version, or a `depend` action of unknown type or without
    `fmri`, is kept as unverifiable, reported by status, and blocks hiding
    or excluding versions of every stem it may need (every stem if it names
    none). Snapshot `require`/`require-any` dependencies that no version
    satisfies (under the snapshot's incorporations) are reported as
    snapshot problems; one only obsolete versions match follows pkg5
    obsoletion and is not.
15. **Restore restarts the clock.** A restored version's inactivity counts
    from its latest restore audit record.

## Consequences

- Hiding costs a catalog rebuild of the affected publishers (ADR-007) and a
  full manifest scan per plan/apply (and per import that excludes
  something). Acceptable for operator commands; an index is follow-up work.
- The legacy pkg5 client's catalog refresh after a switch downloads the full
  catalog (ADR-007, unchanged).
- A pkg5 client that already has a hidden version installed keeps it; exact
  fetches for `pkg fix`/`pkg verify` work. Solver-visible alternatives are
  whatever stays in the catalog: only superseded, inactive, unprotected
  versions leave it.
- Access records under `retention/access/` are written by the depot, which
  therefore needs write access to that directory (not to the repository
  content). Caching proxies in front of the depot, or serving the
  repository by other means, hide accesses from it.
- A depot crash, a failed write or lost access records restart the
  inactivity clock of every version (from the gap's end). A depot that
  stopped without a clean shutdown and does not come back blocks policy
  retirement until an operator runs `retire close-depot`. Two depots with
  the same host name and listen address (an NFS-shared repository on
  identically named hosts) keep closing each other's runs: retirement stays
  blocked (fails closed) until they are named apart.
- Apply of policy candidates waits up to 30 s for depots to confirm the
  fence. Recording is proven against the depots' own view: a record the
  kernel accepted and `fdatasync` confirmed but storage later lost is not
  detected unless its log line or a heartbeat goes missing.
- Snapshot closures follow pkg5 dependency semantics as implemented here;
  they have not yet been validated with real 2020+ OpenIndiana ISO clients
  (#42). Snapshot inventories from ISOs are not produced by this change.
- Purge (stage two), purge-grace enforcement and payload reference
  accounting for deletion remain open (#39/#40).
