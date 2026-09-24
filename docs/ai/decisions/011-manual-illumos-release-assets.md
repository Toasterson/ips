# ADR-011: Suspend the illumos Release Job; Build illumos Assets by Hand

**Date:** 2026-09-24
**Status:** Accepted
**Issues:** PR #46
**Related:** [ADR-005](005-release-pipeline-and-ips-packaging.md) (amends
its illumos job)

## Context

ADR-005 runs one release pipeline with a Linux and an illumos job. The
illumos job repeats the full release build and test run on a Solstice VM
(8 GB, no swap, `-j2`), then installs, upgrades and removes the IPS package
in that throwaway VM's live image. For `v1.0.0-rc.11` it did not finish: the
disposable VM and its package gate are not reliable yet. The owner decided
(2026-09-24) to cut duplicate CI testing and build the illumos release
assets on oibuild until the gate is repaired.

oibuild is a shared OpenIndiana build zone. It can build and run the
unprivileged pkg5 smoke. It must not run the live-image test, which changes
the live system, and it must not hold release credentials.

## Decision

- `release.yml` keeps one pipeline for alpha, beta, rc and stable tags, now
  with only the Linux job. Its gates and the uploader's rules (draft first,
  immutable assets, pre-release published once both platforms are present,
  stable never published by CI) are unchanged. Branch CI is unchanged.
- A maintainer builds the illumos assets from a fresh clone of the exact tag
  on oibuild with `-j2`: tag and version verification, script self-tests,
  release build, nextest inventory, the full unfiltered release-profile
  nextest run, the IPS package build, the native pkg5 smoke
  (`test-pkg5-unprivileged.sh`), tarball, checksums and provenance.
- The assets are copied to a trusted machine, checked against the tag, and
  uploaded there with the same `upload-release-assets.sh`. It gains:
  - `RELEASE_AUTH=basic` with `RELEASE_BASIC_USER` and
    `RELEASE_BASIC_PASSWORD`: HTTP Basic credentials read from 1Password
    into the uploader's environment for one command. In this mode both must
    be non-empty and the script never falls back to a token, so two failed
    `op read`s stop it even with a stray `FORGEJO_TOKEN` in the shell. The
    script copies the pair into unexported shell variables and unsets the
    environment names before it starts any child process, and feeds curl a
    `user` config on stdin (`--config -`), so the script puts neither the
    plaintext nor the base64 form on a command line, in a file, in a child's
    environment or in its output. The script's own initial environment
    still holds the pair while it runs (readable by the same user or root),
    so the upload runs only on a trusted single-user machine. No netrc or
    other file is used, because the 1Password service-account rule forbids
    persisting retrieved secrets anywhere. Without `RELEASE_AUTH` the pair
    still wins over token variables, and the CI token path
    (`RELEASE_TOKEN`, else `FORGEJO_TOKEN`) is unchanged.
  - `RELEASE_NOTES_FILE`: a builder/gate statement. For illumos it is
    required and must state the live-image/SMF result as `NOT RUN` or
    `passed` on a line naming both; that is checked before any network
    call. The draft body becomes the generated text plus the statement,
    after the immutability checks and before any upload, so the upload that
    completes a pre-release cannot publish it without the statement, and a
    stale "Built by Solstice CI" body from an older draft is replaced. Only
    a draft's body is changed.
  - Tags cut before this change (`v1.0.0-rc.11`) carry the old uploader.
    For them the maintainer runs the new script by absolute path from the
    branch that carries it, with the tag's detached checkout as the current
    directory: git verifies the tag there, and `release-lib.sh` comes from
    the new script's directory. Later tags use their own script.
- The live-image install/upgrade/uninstall/SMF test does not run. Every
  release body states that as **NOT RUN**. A stable release is not
  published with that gap open unless the test is run on a disposable VM
  against the released `.p5p` or the owner accepts the gap in the body.

## Consequences

- Tag pipelines are shorter. illumos assets appear only after a maintainer
  finishes the manual build; until then the release stays a draft.
- illumos provenance now depends on the maintainer's process, not on CI.
  The provenance file, checksums and the upload's tag re-verification still
  bind the assets to the tagged commit.
- The pkg5 smoke covers catalog, install, verify and uninstall into a user
  image, but not the package's SMF service, `pkg6srv`, upgrade or live-image
  uninstall. Those regressions can reach pre-releases unnoticed.
- Restoring the illumos job means repairing the VM gate and re-adding the
  job from git history. Its upload step must then pass a
  `RELEASE_NOTES_FILE` with the live-image/SMF result, which the uploader
  requires for illumos, and `test-native-link-retry.sh` (now limited to
  `ci.yml`, and failing if `release.yml` regains an illumos job) must cover
  the job's retry step again.
