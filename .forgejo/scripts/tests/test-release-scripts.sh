#!/usr/bin/env bash
# Self-tests for the release scripts. Offline and side-effect free: the git
# and Forgejo parts run against throwaway fixtures under a temp directory.
# Runs on macOS (bash 3.2), Linux and illumos. Needs git and jq.
#
# Usage: .forgejo/scripts/tests/test-release-scripts.sh
set -euo pipefail

tests=$(cd "$(dirname "$0")" && pwd)
scripts=$(cd "${tests}/.." && pwd)
root=$(cd "${scripts}/../.." && pwd)
# shellcheck source=.forgejo/scripts/release-lib.sh
. "${scripts}/release-lib.sh"

pass=0
failed=0
ok() { pass=$((pass + 1)); }
bad() { failed=$((failed + 1)); echo "not ok: $*" >&2; }
check() { # check DESC CMD...
  local desc="$1"
  shift
  if "$@" >/dev/null 2>&1; then ok; else bad "${desc}"; fi
}
check_not() {
  local desc="$1"
  shift
  if "$@" >/dev/null 2>&1; then bad "${desc}"; else ok; fi
}
eq() { # eq DESC GOT WANT
  if [ "$2" = "$3" ]; then ok; else bad "$1: got '$2', want '$3'"; fi
}

tmp=$(mktemp -d)
trap 'rm -rf "${tmp}"' EXIT
unset GITHUB_REF GITHUB_SHA RELEASE_TAG_OVERRIDE IPS_PUBLISHER || true

# --- tag classification -----------------------------------------------------
classify() { # classify TAG -> "channel prerelease component branch"
  (release_classify "$1" && echo "${RELEASE_CHANNEL} ${RELEASE_PRERELEASE} ${IPS_COMPONENT_VERSION} ${IPS_BRANCH}")
}
eq "alpha" "$(classify v1.0.0-alpha.1)" "alpha true 1.0.0 0.1.1"
eq "beta" "$(classify v1.0.0-beta.2)" "beta true 1.0.0 0.2.2"
eq "rc" "$(classify v1.0.0-rc.10)" "rc true 1.0.0 0.3.10"
eq "stable" "$(classify v1.0.0)" "stable false 1.0.0 1"
eq "multi-digit" "$(classify v12.30.4)" "stable false 12.30.4 1"
for t in v1.0 v1.0.0.0 1.0.0 v01.0.0 v1.0.0-alpha v1.0.0-alpha.01 v1.0.0-preview.1 \
  v1.0.0-RC.1 v1.0.0-rc.1+build v1.0.0-rc.1-x "v1.0.0 " v; do
  check_not "reject tag '${t}'" release_classify "${t}"
done

# --- tag resolution ---------------------------------------------------------
eq "resolve from GITHUB_REF" "$(GITHUB_REF=refs/tags/v2.0.0-rc.1 release_resolve_tag)" "v2.0.0-rc.1"
check_not "reject branch ref" env GITHUB_REF=refs/heads/main bash -c ". '${scripts}/release-lib.sh'; release_resolve_tag"
check_not "reject missing ref" env -u GITHUB_REF bash -c ". '${scripts}/release-lib.sh'; release_resolve_tag"

# --- IPS version ordering ---------------------------------------------------
# dotseq_cmp A B: -1, 0 or 1, comparing like pkg(5) DotSequence.
dotseq_cmp() {
  local a="$1." b="$2." x y
  while [ -n "${a}" ] && [ -n "${b}" ]; do
    x=${a%%.*}; y=${b%%.*}; a=${a#*.}; b=${b#*.}
    if [ "${x}" -lt "${y}" ]; then echo -1; return; fi
    if [ "${x}" -gt "${y}" ]; then echo 1; return; fi
  done
  if [ -n "${a}" ]; then echo 1; elif [ -n "${b}" ]; then echo -1; else echo 0; fi
}
ips_version() { (release_classify "$1" && echo "${IPS_COMPONENT_VERSION} ${IPS_BRANCH}"); }
ips_lt() { # ips_lt TAG_A TAG_B: A sorts strictly before B in pkg(5)
  local va vb c
  va=$(ips_version "$1"); vb=$(ips_version "$2")
  c=$(dotseq_cmp "${va% *}" "${vb% *}")
  [ "${c}" = 0 ] && c=$(dotseq_cmp "${va#* }" "${vb#* }")
  [ "${c}" = -1 ]
}
order="v0.9.9 v1.0.0-alpha.1 v1.0.0-alpha.2 v1.0.0-alpha.10 v1.0.0-beta.1 v1.0.0-rc.1 v1.0.0-rc.2 v1.0.0 v1.0.1-alpha.1 v1.0.1 v1.1.0-rc.1 v1.1.0"
prev=""
for t in ${order}; do
  [ -z "${prev}" ] || check "IPS order ${prev} < ${t}" ips_lt "${prev}" "${t}"
  prev="${t}"
done

# --- asset naming -----------------------------------------------------------
release_classify v1.2.3-rc.1
all=$(release_expected_assets)
# linux: tarball + sums + provenance; illumos: tarball, p5p, p5m + sums + provenance
eq "asset count" "$(printf '%s\n' "${all}" | wc -l | tr -d ' ')" 8
eq "asset names unique" "$(printf '%s\n' "${all}" | sort -u | wc -l | tr -d ' ')" 8
for a in ${all}; do
  case "${a}" in *1.2.3-rc.1* | SHA256SUMS-* | provenance-*) ok ;; *) bad "asset ${a} lacks version" ;; esac
done
eq "linux uploads checksums/provenance last" \
  "$(release_platform_assets linux-x86_64 | tail -n 2 | tr '\n' ' ')" "SHA256SUMS-linux-x86_64 provenance-linux-x86_64.json "
eq "illumos ships the p5p" "$(release_platform_assets illumos-x86_64 | grep -c '\.p5p$')" 1
check_not "unknown platform" release_platform_assets sparc
export IPS_PUBLISHER=openindiana.org
eq "fmri" "$(release_ips_fmri 20260923T101500Z)" \
  "pkg://openindiana.org/package/pkg6/depotd@1.2.3,5.11-0.3.1:20260923T101500Z"

# --- released binaries vs version-checked crates -----------------------------
# v1 ships the depot and operator tools only; pkg6 stays version-checked.
eq "version-checked crates" "${RELEASE_CRATES}" "pkg6 pkg6repo pkg6recv pkg6depotd pkgtree"
eq "released binaries" "${RELEASE_BINARIES}" "pkg6depotd pkg6repo pkg6recv pkgtree"
for b in ${RELEASE_BINARIES}; do
  case " ${RELEASE_CRATES} " in *" ${b} "*) ok ;; *) bad "released binary ${b} is not version-checked" ;; esac
done
check "tarball packs RELEASE_BINARIES" grep -q '^for b in \${RELEASE_BINARIES}; do$' "${scripts}/package-tarball.sh"
check_not "tarball does not pack RELEASE_CRATES" grep -q 'RELEASE_CRATES' "${scripts}/package-tarball.sh"
# The IPS package delivers every released binary except pkgtree, whose
# usr/bin/pkgtree belongs to OpenIndiana's developer/pkgtree.
eq "IPS package delivers the released binaries except pkgtree" \
  "$(printf '%s\n' pkg6depotd $(sed -n 's/^for b in \(.*\); do$/\1/p' "${scripts}/build-ips-package.sh") | sort | tr '\n' ' ')" \
  "$(printf '%s\n' ${RELEASE_BINARIES} | grep -vx pkgtree | sort | tr '\n' ' ')"
if command -v gtar >/dev/null 2>&1; then tar_cmd=gtar; else tar_cmd=tar; fi
if "${tar_cmd}" --version 2>/dev/null | grep -q 'GNU tar'; then
  # End to end: fake binaries for every crate, pkg6 included; only the released
  # ones may end up in the tarball.
  tb="${tmp}/tarball"
  git init -q "${tb}"
  mkdir -p "${tb}/target/release" "${tb}/out"
  for b in ${RELEASE_CRATES}; do echo "${b}" > "${tb}/target/release/${b}"; done
  echo l > "${tb}/LICENSE" && echo r > "${tb}/README.md"
  (cd "${tb}" && git add LICENSE README.md && GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.invalid \
    GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.invalid git commit -qm a)
  (cd "${tb}" && GITHUB_REF=refs/tags/v1.2.3 CARGO_TARGET_DIR="${tb}/target" \
    "${scripts}/package-tarball.sh" linux-x86_64 "${tb}/out" >/dev/null)
  eq "tarball contents" \
    "$("${tar_cmd}" -tzf "${tb}/out/$(release_classify v1.2.3 && release_tarball_name linux-x86_64)" | tr '\n' ' ')" \
    "ips-1.2.3-linux-x86_64/ ips-1.2.3-linux-x86_64/LICENSE ips-1.2.3-linux-x86_64/README.md ips-1.2.3-linux-x86_64/bin/ ips-1.2.3-linux-x86_64/bin/pkg6depotd ips-1.2.3-linux-x86_64/bin/pkg6recv ips-1.2.3-linux-x86_64/bin/pkg6repo ips-1.2.3-linux-x86_64/bin/pkgtree "
else
  echo "skip: GNU tar not found, tarball contents not built" >&2
fi

# --- exact tag -> commit provenance -----------------------------------------
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.invalid GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.invalid
export GIT_AUTHOR_DATE="2026-09-23T10:15:00Z" GIT_COMMITTER_DATE="2026-09-23T10:15:00Z"
origin="${tmp}/origin.git"
work="${tmp}/work"
git init -q --bare "${origin}"
git init -q "${work}"
(
  cd "${work}"
  git remote add origin "${origin}"
  echo a > f && echo 'lock' > Cargo.lock && git add f Cargo.lock && git commit -qm a
  git tag -a -m "rc" v1.2.3-rc.1
  git tag v1.2.3-rc.2
  echo b > f && git commit -qam b
  git push -q origin HEAD:refs/heads/main --tags
)
# CI-like checkout: shallow, no tags, detached at the tagged commit.
ci="${tmp}/ci"
tagged=$(git -C "${work}" rev-parse "v1.2.3-rc.1^{commit}")
git init -q "${ci}"
git -C "${ci}" remote add origin "file://${origin}"
git -C "${ci}" fetch -q --depth=1 --no-tags origin "${tagged}"
git -C "${ci}" checkout -q --detach "${tagged}"

verify() { # verify TAG [GITHUB_SHA]: run the provenance checks in the CI clone
  (cd "${ci}" && GITHUB_REF="refs/tags/$1" GITHUB_SHA="${2:-}" bash -c "
    set -euo pipefail; . '${scripts}/release-lib.sh'
    release_init; release_fetch_tag; release_verify_commit
    echo \"\${RELEASE_COMMIT} \${RELEASE_TAG_TYPE} \$(release_ips_timestamp)\"")
}
eq "annotated tag verifies" "$(verify v1.2.3-rc.1 "${tagged}" 2>/dev/null)" "${tagged} tag 20260923T101500Z"
eq "lightweight tag verifies" "$(verify v1.2.3-rc.2 2>/dev/null)" "${tagged} commit 20260923T101500Z"
tag_obj=$(git -C "${work}" rev-parse v1.2.3-rc.1)
eq "annotated tag: GITHUB_SHA may be the tag object" "$(verify v1.2.3-rc.1 "${tag_obj}" 2>/dev/null)" "${tagged} tag 20260923T101500Z"
check_not "GITHUB_SHA mismatch fails" verify v1.2.3-rc.1 0000000000000000000000000000000000000000
check_not "GITHUB_SHA of another tag object fails" verify v1.2.3-rc.2 "${tag_obj}0"
(cd "${work}" && git tag v1.2.3 HEAD && git push -q origin v1.2.3)
check_not "tag on another commit fails" verify v1.2.3
(cd "${work}" && git tag -f -a -m moved v1.2.3-rc.1 HEAD >/dev/null && git push -q -f origin v1.2.3-rc.1)
check_not "tag moved on origin fails" bash -c "cd '${ci}' && GITHUB_REF=refs/tags/v1.2.3-rc.1 bash -c '
  . \"${scripts}/release-lib.sh\"; release_init && release_verify_commit'"
check_not "missing tag fails" verify v9.9.9
check_not "archive checkout (no .git) fails" bash -c "cd '${tmp}' && GITHUB_REF=refs/tags/v1.2.3-rc.2 bash -c '
  . \"${scripts}/release-lib.sh\"; release_init && release_fetch_tag'"

# --- uploader: draft/prerelease/immutability against a fake Forgejo -----------
mkdir -p "${tmp}/bin"
ln -s "${tests}/fake-curl" "${tmp}/bin/curl"
# illumos assets are built by hand and need a builder/gate statement.
printf 'illumos-x86_64: built on oibuild by hand.\nLive-image install/upgrade/uninstall/SMF: NOT RUN.\n' > "${tmp}/notes.md"
upload() { # upload STATE TAG PLATFORM DIR
  # UPLOAD_NOTES, UPLOAD_AUTH, UPLOAD_BASIC_USER, UPLOAD_BASIC_PASSWORD,
  # UPLOAD_FORGEJO_TOKEN and UPLOAD_RELEASE_TOKEN override the defaults: CI's
  # FORGEJO_TOKEN, and notes for illumos only. FAKE_FORGE_BASIC tells
  # fake-curl the expected pair.
  local notes="${UPLOAD_NOTES-}"
  [ "$3" != illumos-x86_64 ] || notes="${UPLOAD_NOTES-${tmp}/notes.md}"
  (cd "${ci}" && PATH="${tmp}/bin:${PATH}" FAKE_FORGE_STATE="$1" GITHUB_REF="refs/tags/$2" \
    FORGE_BASE=http://forge.invalid REPO_OWNER=o REPO_NAME=r \
    FORGEJO_TOKEN="${UPLOAD_FORGEJO_TOKEN-x}" RELEASE_TOKEN="${UPLOAD_RELEASE_TOKEN-}" \
    RELEASE_AUTH="${UPLOAD_AUTH-}" \
    RELEASE_BASIC_USER="${UPLOAD_BASIC_USER-}" RELEASE_BASIC_PASSWORD="${UPLOAD_BASIC_PASSWORD-}" \
    FAKE_FORGE_BASIC="${UPLOAD_BASIC_USER-}:${UPLOAD_BASIC_PASSWORD-}" RELEASE_NOTES_FILE="${notes}" \
    "${scripts}/upload-release-assets.sh" "$3" "$4")
}
make_assets() { # make_assets TAG PLATFORM DIR [salt]
  (release_classify "$1" && mkdir -p "$3" && for a in $(release_platform_assets "$2"); do
    echo "$1 $2 ${a} ${4:-}" > "$3/${a}"; done)
}
rel_field() { jq -r ".[] | select(.tag_name == \"$2\") | $3" "$1/releases.json"; }

# The rc.2 tag still points at the CI checkout's commit.
st="${tmp}/forge-rc"
mkdir -p "${st}"
make_assets v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"
make_assets v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos"
check "prerelease: first platform uploads" upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"
eq "prerelease stays draft until complete" "$(rel_field "${st}" v1.2.3-rc.2 .draft)" true
eq "prerelease flag set" "$(rel_field "${st}" v1.2.3-rc.2 .prerelease)" true
eq "target is the tagged commit" "$(rel_field "${st}" v1.2.3-rc.2 .target_commitish)" "${tagged}"
check "prerelease: second platform uploads" upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos"
eq "complete prerelease is published" "$(rel_field "${st}" v1.2.3-rc.2 .draft)" false
eq "all assets present" "$(rel_field "${st}" v1.2.3-rc.2 '.assets | length')" 8
check "identical re-run is a no-op" upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos"
make_assets v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos2" rebuilt
check_not "different bytes are refused" upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos2"
check_not "nothing was ever deleted" grep -q DELETE "${st}/log"
eq "uploads happened once per asset" "$(grep -c '^POST' "${st}/log")" 8
# The notes reach the draft body before the first illumos asset, and the
# publish comes last.
notes_at=$(grep -n '^PATCH .*"body"' "${st}/log" | head -1 | cut -d: -f1)
first_illumos=$(grep -n '^POST .*illumos' "${st}/log" | head -1 | cut -d: -f1)
publish_at=$(grep -n '^PATCH .*"draft": false' "${st}/log" | head -1 | cut -d: -f1)
check "notes written before the first illumos asset" [ -n "${notes_at}" ] && [ "${notes_at}" -lt "${first_illumos}" ]
last_asset=$(grep -n '^POST' "${st}/log" | tail -1 | cut -d: -f1)
check "pre-release published after the last asset" [ "${publish_at}" -gt "${last_asset}" ]
case "$(rel_field "${st}" v1.2.3-rc.2 .body)" in
  *"Built from tag"*"Live-image install/upgrade/uninstall/SMF: NOT RUN."*) ok ;;
  *) bad "release body lacks the generated text and the notes" ;;
esac
eq "notes written once" "$(grep -c '^PATCH .*"body"' "${st}/log")" 1
eq "CI path authenticated with the token on every call" "$(sort -u "${st}/auth")" token

# A stable tag on the same commit: complete, but never leaves draft.
(cd "${work}" && git tag v1.2.4 "${tagged}" && git push -q origin v1.2.4)
st="${tmp}/forge-ga"
mkdir -p "${st}"
make_assets v1.2.4 linux-x86_64 "${tmp}/ga/linux"
make_assets v1.2.4 illumos-x86_64 "${tmp}/ga/illumos"
check "stable: linux uploads" upload "${st}" v1.2.4 linux-x86_64 "${tmp}/ga/linux"
check "stable: illumos uploads" upload "${st}" v1.2.4 illumos-x86_64 "${tmp}/ga/illumos"
eq "stable is not a prerelease" "$(rel_field "${st}" v1.2.4 .prerelease)" false
eq "complete stable release stays draft" "$(rel_field "${st}" v1.2.4 .draft)" true
check_not "stable was never taken out of draft" grep -q '^PATCH .*"draft"' "${st}/log"
eq "stable: only the notes were patched" "$(grep -c '^PATCH' "${st}/log")" 1

# A release someone already published by hand gets no new assets.
st="${tmp}/forge-published"
mkdir -p "${st}"
echo '[{"id":1,"tag_name":"v1.2.4","draft":false,"prerelease":false,"assets":[]}]' > "${st}/releases.json"
check_not "published release is not modified" upload "${st}" v1.2.4 linux-x86_64 "${tmp}/ga/linux"
# A hand-made release whose prerelease flag contradicts the tag is refused.
echo '[{"id":1,"tag_name":"v1.2.4","draft":true,"prerelease":true,"assets":[]}]' > "${st}/releases.json"
check_not "prerelease flag mismatch is refused" upload "${st}" v1.2.4 linux-x86_64 "${tmp}/ga/linux"
check_not "missing build output is refused" upload "${st}" v1.2.4 linux-x86_64 "${tmp}/empty"

# Regression: a failing release list must end the run, not loop.
st="${tmp}/forge-list-fails"
mkdir -p "${st}"
upload_list_fails() { FAKE_FORGE_FAIL_LIST=1 upload "$@"; }
check_not "release list HTTP 500 fails the upload" upload_list_fails "${st}" v1.2.4 linux-x86_64 "${tmp}/ga/linux"
calls=$(cat "${st}/calls" 2>/dev/null || echo 0)
check "release list failure stops after one call (made ${calls})" [ "${calls}" -le 2 ]
check_not "nothing uploaded after list failure" grep -q '^POST' "${st}/log"

# Download failure and hash mismatch are distinct errors.
# Capture only stderr: the order (stderr to the pipe, then stdout away) is intended.
# shellcheck disable=SC2069
upload_err() { upload "$@" 2>&1 >/dev/null; }
st="${tmp}/forge-unreadable"
mkdir -p "${st}"
lin=$(release_classify v1.2.4 && release_tarball_name linux-x86_64)
jq -n --arg n "${lin}" '[{id: 1, tag_name: "v1.2.4", draft: true, prerelease: false,
  assets: [{name: $n, size: 1, browser_download_url: "file:///nonexistent/asset", uuid: ""}]}]' > "${st}/releases.json"
err=$(upload_err "${st}" v1.2.4 linux-x86_64 "${tmp}/ga/linux" || true)
case "${err}" in *"could not be downloaded to compare"*) ok ;; *) bad "download failure not reported as such: ${err}" ;; esac
check_not "no upload when an existing asset is unverifiable" grep -q '^POST' "${st}/log"
st="${tmp}/forge-mismatch"
mkdir -p "${st}/blobs/1"
echo other > "${st}/blobs/1/${lin}"
jq -n --arg n "${lin}" --arg u "file://${st}/blobs/1/${lin}" '[{id: 1, tag_name: "v1.2.4", draft: true,
  prerelease: false, assets: [{name: $n, size: 6, browser_download_url: $u, uuid: ""}]}]' > "${st}/releases.json"
err=$(upload_err "${st}" v1.2.4 linux-x86_64 "${tmp}/ga/linux" || true)
case "${err}" in *"with different content"*) ok ;; *) bad "mismatch not reported as such: ${err}" ;; esac
case "${err}" in *"delete its assets"*) ok ;; *) bad "mismatch error lacks cleanup guidance" ;; esac
check_not "no partial upload after a mismatch" grep -q '^POST' "${st}/log"

# Clean re-run: every asset already uploaded but the publish step never ran
# (e.g. the PATCH failed); re-running either platform finalizes.
st="${tmp}/forge-finalize"
mkdir -p "${st}"
check "finalize: linux" upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"
check "finalize: illumos" upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos"
jq 'map(.draft = true)' "${st}/releases.json" > "${st}/r" && mv "${st}/r" "${st}/releases.json"
check "finalize: identical re-run" upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"
eq "re-run publishes the complete pre-release" "$(rel_field "${st}" v1.2.3-rc.2 .draft)" false
eq "re-run uploaded nothing new" "$(grep -c '^POST' "${st}/log")" 8

with_env() { # with_env VAR=VALUE... -- CMD...: run CMD (a function too) with VARs set
  (while [ "$1" != -- ]; do export "$1"; shift; done; shift; "$@")
}

# illumos assets are refused without a builder/gate statement; nothing moves.
st="${tmp}/forge-nonotes"
mkdir -p "${st}"
check_not "illumos without RELEASE_NOTES_FILE is refused" with_env UPLOAD_NOTES='' -- upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos"
check_not "empty notes file is refused" with_env UPLOAD_NOTES=/dev/null -- upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos"
check_not "no release call without notes" [ -s "${st}/calls" ]

# The notes must give the live-image/SMF gate result, NOT RUN or passed, on
# a line naming both; checked before any call.
st="${tmp}/forge-gate"
mkdir -p "${st}"
gate_notes() { printf '### illumos-x86_64\n\nBuilt by hand on oibuild.\n%s\n' "$1" > "${tmp}/gate-notes.md"; }
gate_notes 'Release build: passed.'
check_not "notes without a live-image/SMF line are refused" with_env UPLOAD_NOTES="${tmp}/gate-notes.md" -- upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos"
gate_notes 'Live-image install/upgrade/uninstall/SMF: see below.'
check_not "a live-image/SMF line without a verdict is refused" with_env UPLOAD_NOTES="${tmp}/gate-notes.md" -- upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos"
gate_notes 'Live-image install and SMF: passed, NOT RUN on upgrade.'
check_not "a line with both verdicts is refused" with_env UPLOAD_NOTES="${tmp}/gate-notes.md" -- upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos"
gate_notes "$(printf '| live-image SMF | passed |\n| live-image SMF | **NOT RUN** |')"
check_not "conflicting verdict lines are refused" with_env UPLOAD_NOTES="${tmp}/gate-notes.md" -- upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos"
check_not "no release call without a gate verdict" [ -s "${st}/calls" ]
# The v1.0.0-rc.11 wording, the documented table row and a passed run are
# accepted, and the upload goes through.
n=0
for line in 'Live-image IPS install, upgrade, uninstall and SMF lifecycle validation: NOT RUN in the oibuild validation' \
  '| live-image install/upgrade/uninstall/SMF (`test-ips-package.sh`) | **NOT RUN** |' \
  'Live-image install/upgrade/uninstall/SMF on a disposable VM: passed.'; do
  n=$((n + 1))
  st="${tmp}/forge-gate-ok${n}"
  mkdir -p "${st}"
  gate_notes "${line}"
  check "gate verdict accepted: ${line}" with_env UPLOAD_NOTES="${tmp}/gate-notes.md" -- upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos"
done

# A published release's body is never changed: the script reaches the
# release (list, then its assets) and stops without a write.
st="${tmp}/forge-pub-body"
mkdir -p "${st}"
echo '[{"id":1,"tag_name":"v1.2.3-rc.2","draft":false,"prerelease":true,"body":"old","assets":[]}]' > "${st}/releases.json"
check_not "notes on a published release are refused" upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos"
check "published: release list requested" grep -qF -- "-X GET http://forge.invalid/api/v1/repos/o/r/releases?limit=50&page=1" "${st}/argv"
check "published: its assets requested" grep -qF -- "-X GET http://forge.invalid/api/v1/repos/o/r/releases/1/assets" "${st}/argv"
check_not "published: no PATCH or POST sent" grep -qE -- '-X (PATCH|POST) ' "${st}/argv"
check_not "published: nothing written" [ -e "${st}/log" ]
eq "published body untouched" "$(rel_field "${st}" v1.2.3-rc.2 .body)" old

# A draft that an earlier CI run created still says "Built by Solstice CI".
# Its body is replaced with the generated text plus the disclosure before
# the first illumos asset goes up.
st="${tmp}/forge-ci-body"
mkdir -p "${st}"
echo '[{"id":1,"tag_name":"v1.2.3-rc.2","draft":true,"prerelease":true,"body":"Built by Solstice CI from tag `v1.2.3-rc.2`.","assets":[]}]' > "${st}/releases.json"
check "CI draft body: illumos uploads" upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos"
notes_at=$(grep -n '^PATCH .*"body"' "${st}/log" | head -1 | cut -d: -f1)
first_illumos=$(grep -n '^POST .*illumos' "${st}/log" | head -1 | cut -d: -f1)
check "CI draft body: replaced before the first illumos asset" [ -n "${notes_at}" ] && [ -n "${first_illumos}" ] && [ "${notes_at}" -lt "${first_illumos}" ]
case "$(rel_field "${st}" v1.2.3-rc.2 .body)" in
  *"Built by Solstice CI"*) bad "CI draft body: stale Solstice claim kept" ;;
  *"Built from tag"*"Live-image install/upgrade/uninstall/SMF: NOT RUN."*) ok ;;
  *) bad "CI draft body: lacks the generated text and the disclosure" ;;
esac
eq "CI draft body: still a draft" "$(rel_field "${st}" v1.2.3-rc.2 .draft)" true

# Local maintainer path: HTTP Basic from two environment variables. It wins
# over a stray FORGEJO_TOKEN; the pair reaches curl only on stdin, never its
# argv, its environment, the output or a file. The password carries the two
# characters curl config strings escape. Everything stays in ${tmp}.
basic_pw='s3cr3t-"basic\pw'
st="${tmp}/forge-basic"
mkdir -p "${st}"
out=$(UPLOAD_BASIC_USER=maint UPLOAD_BASIC_PASSWORD="${basic_pw}" UPLOAD_FORGEJO_TOKEN=stray-token \
  upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux" 2>&1) && ok || bad "basic: linux upload failed: ${out}"
out2=$(UPLOAD_BASIC_USER=maint UPLOAD_BASIC_PASSWORD="${basic_pw}" UPLOAD_FORGEJO_TOKEN=stray-token \
  upload "${st}" v1.2.3-rc.2 illumos-x86_64 "${tmp}/rc/illumos" 2>&1) && ok || bad "basic: illumos upload failed: ${out2}"
out="${out}${out2}"
eq "basic: pre-release published" "$(rel_field "${st}" v1.2.3-rc.2 .draft)" false
eq "basic: every call sent the pair on stdin, none leaked into curl's env" "$(sort -u "${st}/auth")" basic
check_not "basic: password not on curl argv" grep -qF "s3cr3t" "${st}/argv"
check_not "basic: stray token not used" grep -qF stray-token "${st}/argv"
check_not "basic: password in no state file" grep -rqF "s3cr3t" "${st}"
case "${out}" in *s3cr3t*) bad "basic: password printed" ;; *) ok ;; esac
if command -v base64 >/dev/null; then
  b64=$(printf 'maint:%s' "${basic_pw}" | base64 | tr -d '\n')
  check_not "basic: base64 credentials not on curl argv" grep -qF "${b64}" "${st}/argv"
  case "${out}" in *"${b64}"*) bad "basic: base64 credentials printed" ;; *) ok ;; esac
fi
st="${tmp}/forge-basic-half"
mkdir -p "${st}"
check_not "basic: user without password is refused" with_env UPLOAD_BASIC_USER=maint -- upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"
check_not "basic: password without user is refused" with_env UPLOAD_BASIC_PASSWORD=pw -- upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"
check_not "basic: user with ':' is refused" with_env UPLOAD_BASIC_USER=a:b UPLOAD_BASIC_PASSWORD=pw -- upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"
check_not "basic: no call with an incomplete pair" [ -s "${st}/calls" ]

# RELEASE_AUTH=basic (the documented maintainer command): a pair that came
# out empty, for example two failed 1Password reads, is refused even with a
# token in the environment; it never falls back.
st="${tmp}/forge-auth-basic-empty"
mkdir -p "${st}"
check_not "RELEASE_AUTH=basic: empty pair refused despite FORGEJO_TOKEN" with_env UPLOAD_AUTH=basic UPLOAD_FORGEJO_TOKEN=stray UPLOAD_RELEASE_TOKEN=stray -- upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"
check_not "RELEASE_AUTH=basic: user alone refused" with_env UPLOAD_AUTH=basic UPLOAD_BASIC_USER=maint -- upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"
check_not "RELEASE_AUTH=basic: password alone refused" with_env UPLOAD_AUTH=basic UPLOAD_BASIC_PASSWORD=pw -- upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"
check_not "RELEASE_AUTH: unknown mode refused" with_env UPLOAD_AUTH=token -- upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"
check_not "RELEASE_AUTH=basic: no call when refused" [ -s "${st}/calls" ]
# With a pair it uploads with Basic only. Spies on dirname, git and jq, the
# script's first child processes, prove none inherits the pair.
mkdir -p "${tmp}/spy"
for t in dirname git jq; do
  printf '#!/bin/sh
if [ -n "${RELEASE_BASIC_USER+x}${RELEASE_BASIC_PASSWORD+x}" ]; then echo %s >> "${FAKE_FORGE_STATE}/child-leak"; fi
exec %s "$@"
' \
    "${t}" "$(command -v "${t}")" > "${tmp}/spy/${t}"
  chmod +x "${tmp}/spy/${t}"
done
st="${tmp}/forge-auth-basic"
mkdir -p "${st}"
check "RELEASE_AUTH=basic: pair uploads" with_env PATH="${tmp}/spy:${PATH}" UPLOAD_AUTH=basic UPLOAD_BASIC_USER=maint UPLOAD_BASIC_PASSWORD=pw UPLOAD_FORGEJO_TOKEN=stray-token -- upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"
eq "RELEASE_AUTH=basic: Basic on every call" "$(sort -u "${st}/auth")" basic
check_not "RELEASE_AUTH=basic: stray token not used" grep -qF stray-token "${st}/argv"
check_not "RELEASE_AUTH=basic: no child inherited the pair" [ -e "${st}/child-leak" ]

# The repository secret RELEASE_TOKEN alone still works, and no credential
# at all is refused.
st="${tmp}/forge-release-token"
mkdir -p "${st}"
check "RELEASE_TOKEN alone uploads" with_env UPLOAD_FORGEJO_TOKEN='' UPLOAD_RELEASE_TOKEN=rt -- upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"
eq "RELEASE_TOKEN: token auth" "$(sort -u "${st}/auth")" token
st="${tmp}/forge-nocreds"
mkdir -p "${st}"
check_not "no credentials are refused" with_env UPLOAD_FORGEJO_TOKEN='' -- upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/rc/linux"

# Provenance is a function of source and toolchain only: two runs with
# different runner context write identical bytes, and the second run's
# upload is a no-op.
if command -v rustc >/dev/null 2>&1 && command -v cargo >/dev/null 2>&1; then
  prov_run() { # prov_run OUTDIR REQUEST_ID GITHUB_SHA
    mkdir -p "$1"
    printf 'tarball bytes\n' > "$1/$(release_classify v1.2.3-rc.2 && release_tarball_name linux-x86_64)"
    (cd "${ci}" && GITHUB_REF=refs/tags/v1.2.3-rc.2 SOLSTICE_REQUEST_ID="$2" GITHUB_SHA="$3" \
      FORGE_BASE=http://forge.invalid REPO_OWNER=o REPO_NAME=r \
      "${scripts}/write-provenance.sh" linux-x86_64 "$1" "$1/$(release_classify v1.2.3-rc.2 && release_tarball_name linux-x86_64)") >/dev/null
  }
  prov_run "${tmp}/prov1" request-1 "${tagged}"
  prov_run "${tmp}/prov2" request-2 ""
  check "provenance identical across runs" cmp "${tmp}/prov1/provenance-linux-x86_64.json" "${tmp}/prov2/provenance-linux-x86_64.json"
  check "checksums identical across runs" cmp "${tmp}/prov1/SHA256SUMS-linux-x86_64" "${tmp}/prov2/SHA256SUMS-linux-x86_64"
  check_not "provenance carries no host name" grep -qF "$(uname -n)" "${tmp}/prov1/provenance-linux-x86_64.json"
  eq "provenance names the tagged commit" "$(jq -r .source.commit "${tmp}/prov1/provenance-linux-x86_64.json")" "${tagged}"
  eq "provenance repository is the canonical public URL" \
    "$(jq -r .source.repository "${tmp}/prov1/provenance-linux-x86_64.json")" "http://forge.invalid/o/r.git"
  st="${tmp}/forge-prov"
  mkdir -p "${st}"
  check "provenance run 1 uploads" upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/prov1"
  check "provenance run 2 re-uploads cleanly" upload "${st}" v1.2.3-rc.2 linux-x86_64 "${tmp}/prov2"
  eq "provenance re-run uploaded nothing new" "$(grep -c '^POST' "${st}/log")" 3
else
  echo "skip: provenance determinism (no rustc/cargo)"
fi

# --- public repository URL never carries credentials --------------------------
urlrepo="${tmp}/urlrepo"
git init -q "${urlrepo}"
git -C "${urlrepo}" remote add origin "https://ci-user:t0ken-secret@code.aopc.cloud/OpenIndiana/ips.git"
repo_url() { (cd "${urlrepo}" && unset FORGE_BASE REPO_OWNER REPO_NAME && release_public_repo_url); }
eq "credentials stripped from origin URL" "$(repo_url)" "https://code.aopc.cloud/OpenIndiana/ips.git"
git -C "${urlrepo}" remote set-url origin "https://t0ken-secret@code.aopc.cloud/OpenIndiana/ips.git"
eq "token-only userinfo stripped" "$(repo_url)" "https://code.aopc.cloud/OpenIndiana/ips.git"
eq "canonical URL from workflow env" \
  "$(cd "${urlrepo}" && FORGE_BASE=https://code.aopc.cloud/ REPO_OWNER=OpenIndiana REPO_NAME=ips release_public_repo_url)" \
  "https://code.aopc.cloud/OpenIndiana/ips.git"

# --- manifest macro check is semantic -----------------------------------------
# Emulate pkgmogrify -D for exactly the macros build-ips-package.sh defines.
p5m="${root}/packaging/ips/pkg6depotd.p5m"
build="${scripts}/build-ips-package.sh"
mogrify_like_build() {
  local args="" m
  for m in $(sed -n 's/^  -D "\([A-Z_]*\)=.*/\1/p' "${build}"); do
    args="${args} -e s|\$(${m})|x|g"
  done
  # shellcheck disable=SC2086
  sed ${args} "$1"
}
mogrify_like_build "${p5m}" > "${tmp}/expanded.p5m"
eq "template fully expands with the build's macros" "$(release_unexpanded_macros "${tmp}/expanded.p5m")" ""
printf '# a comment mentioning $(MACRO) is fine\nset name=a value=b\n' > "${tmp}/comment.p5m"
eq "macros in comments are ignored" "$(release_unexpanded_macros "${tmp}/comment.p5m")" ""
printf 'set name=pkg.fmri value=$(NOT_DEFINED)\n' > "${tmp}/bad.p5m"
check "unexpanded macro in an action is reported" grep -q 'NOT_DEFINED' <(release_unexpanded_macros "${tmp}/bad.p5m")
check "build aborts on unexpanded macros via the semantic check" grep -q 'release_unexpanded_macros' "${build}"

# --- SMF FMRI comes from pkgdepend only ----------------------------------------
eq "template sets no org.opensolaris.smf.fmri (pkgdepend adds it)" "$(release_smf_fmri_actions "${p5m}")" 0
printf 'set name=org.opensolaris.smf.fmri value=svc:/a value=svc:/a:default\n' > "${tmp}/gen.p5m"
eq "smf fmri counter" "$(release_smf_fmri_actions "${tmp}/gen.p5m")" 1
check "build requires exactly one generated smf fmri" grep -q 'expected one generated org.opensolaris.smf.fmri' "${build}"

# --- scratch-repo verify checks integrity, not OS dependencies ------------------
check "pkgrepo verify skips dependency resolution in the scratch repo" \
  grep -q '^pkgrepo verify --disable dependency -s "${repo}"$' "${build}"
check "live install test still resolves dependencies" grep -q 'sudo pkg install -v -g' "${scripts}/test-ips-package.sh"

# --- published FMRI keeps the tagged timestamp ----------------------------------
# Runs build-ips-package.sh against fake pkg5 tools in a throwaway checkout.
# The fakes keep pkg5's layout: repository and p5p both store each manifest
# at publisher/PUB/pkg/STEM/VERSION, %-quoted, declaring the full pkg.fmri.
# The fake pkgsend behaves like pkg5 publish.py: it replaces the manifest's
# timestamp with the publication time unless the global -D allow-timestamp
# precedes the subcommand. The fake pkgrecv --newest prints like pkg5's
# (get_fmri(include_build=False)): no build release. FAKE_PUBLISH_FMRI,
# FAKE_ARCHIVE_FMRI, FAKE_ARCHIVE_EXTRA and FAKE_ARCHIVE_DECLARES make the
# fakes publish or archive something else.
fb="${tmp}/fakebuild"
mkdir -p "${fb}/bin" "${fb}/co/target/release" "${fb}/co/packaging/ips" "${fb}/co/pkg6repo" "${fb}/co/man/man5" "${fb}/co/man/man8"
cat >"${fb}/fake-store.sh" <<'EOF'
# fake_store DIR FMRI: store a manifest for FMRI in the pkg5 layout under DIR.
fake_store() {
  local rest="${2#pkg://}" q
  q() { printf '%s' "$1" | sed -e 's/%/%25/g' -e 's|/|%2F|g' -e 's/,/%2C/g' -e 's/:/%3A/g'; }
  rest="${rest#*/}"
  local d="$1/publisher/${2#pkg://}"
  d="${d%%/"${rest}"}/pkg/$(q "${rest%%@*}")"
  mkdir -p "${d}"
  printf 'set name=pkg.fmri value=%s\nset name=pkg.summary value=x\n' "$2" >"${d}/$(q "${rest#*@}")"
}
EOF
cat >"${fb}/bin/pkgmogrify" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
args=()
while [ "$#" -gt 1 ]; do
  [ "$1" = -D ] && { args+=(-e "s|\$(${2%%=*})|${2#*=}|g"); shift; }
  shift
done
sed "${args[@]}" "$1"
EOF
cat >"${fb}/bin/pkgdepend" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  generate) eval "cat \"\${$#}\"" ;;
  resolve) { cat "$3"
    echo 'set name=org.opensolaris.smf.fmri value=svc:/application/pkg/pkg6depotd value=svc:/application/pkg/pkg6depotd:default'
    echo 'depend fmri=pkg:/system/library@0.5.11 type=require'; } >"$3.res" ;;
esac
EOF
cat >"${fb}/bin/pkglint" <<'EOF'
#!/bin/sh
exit 0
EOF
cat >"${fb}/bin/pkgrepo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in create) mkdir -p "$2" ;; esac
EOF
cat >"${fb}/bin/pkgsend" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
. "$(dirname "$0")/../fake-store.sh"
keep=""
while [ "$1" != publish ]; do
  case "$1" in -D) [ "$2" = allow-timestamp ] && keep=1; shift 2 ;; -s) repo="$2"; shift 2 ;; *) exit 2 ;; esac
done
fmri=$(sed -n 's/^set name=pkg\.fmri value=\([^ ]*\).*/\1/p' "${@: -1}")
[ -n "${keep}" ] || fmri="${fmri%:*}:20991231T235959Z"
fmri="${FAKE_PUBLISH_FMRI:-${fmri}}"
fake_store "${repo}" "${fmri}"
echo "${fmri}"
echo PUBLISHED
EOF
cat >"${fb}/bin/pkgrecv" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
. "$(dirname "$0")/../fake-store.sh"
src="$2" x=$(mktemp -d)
trap 'rm -rf "${x}"' EXIT
case "$3" in
  --newest)
    [ -d "${src}" ] || { (cd "${x}" && tar -xf "${src}"); src="${x}"; }
    find "${src}/publisher" -path '*/pkg/*/*' -type f -exec sed -n 's/^set name=pkg\.fmri value=//p' {} + \
      | sed 's/,[^-]*-/-/' | sort ;;
  -a)
    cp -R "${src}/publisher" "${x}/"
    [ -z "${FAKE_ARCHIVE_FMRI:-}" ] || { rm -rf "${x}/publisher"; fake_store "${x}" "${FAKE_ARCHIVE_FMRI}"; }
    [ -z "${FAKE_ARCHIVE_EXTRA:-}" ] || fake_store "${x}" "${FAKE_ARCHIVE_EXTRA}"
    if [ -n "${FAKE_ARCHIVE_DECLARES:-}" ]; then
      for m in "${x}"/publisher/*/pkg/*/*; do
        sed "s|^set name=pkg\.fmri value=.*|set name=pkg.fmri value=${FAKE_ARCHIVE_DECLARES}|" "${m}" >"${x}/m"
        mv "${x}/m" "${m}"
      done
    fi
    (cd "${x}" && tar -cf "$5" publisher) ;;
  *) exit 2 ;;
esac
EOF
chmod +x "${fb}"/bin/*
cp "${root}/packaging/ips/pkg6depotd.p5m" "${fb}/co/packaging/ips/"
for f in target/release/pkg6depotd target/release/pkg6repo target/release/pkg6recv target/release/pkgtree \
  packaging/ips/pkg6depotd.kdl packaging/ips/pkg6depotd.xml packaging/ips/svc-pkg6depotd packaging/ips/pkg6depotd.8 \
  packaging/ips/pkglintrc pkg6repo/pkg6repo.1 man/man5/pkg6-policy.5 man/man8/pkg6repo-retire.8; do
  echo x >"${fb}/co/${f}"
done
(cd "${fb}/co" && git init -q && git add packaging && \
  GIT_COMMITTER_DATE="2026-09-23T10:21:00Z" git commit -qm fixture)
fakebuild() { # fakebuild SCRIPT [VAR=VALUE...]: build into ${fb}/dist; prints fmri.env
  rm -rf "${fb}/dist" "${fb}/co/target/ips"
  (cd "${fb}/co" && unset CARGO_TARGET_DIR && env PATH="${fb}/bin:${PATH}" \
    RELEASE_TAG_OVERRIDE=v1.0.0-rc.3 RELEASE_ALLOW_UNTAGGED=1 "${@:2}" bash "$1" "${fb}/dist") >"${fb}/log" 2>&1 \
    && cat "${fb}/co/target/ips/fmri.env"
}
fails_closed() { # fails_closed WHAT LOGTEXT: after a failed fakebuild
  check "  ... on the ${1}" grep -qF "$2" "${fb}/log"
  check_not "  ... and writes no fmri.env" test -e "${fb}/co/target/ips/fmri.env"
  check_not "  ... and leaves no archive in dist" test -e "${fb}/dist/pkg6depotd-1.0.0-rc.3-illumos-x86_64.p5p"
}
want="pkg://openindiana.org/package/pkg6/depotd@1.0.0,5.11-0.3.3:20260923T102100Z"
want_path="publisher/openindiana.org/pkg/package%2Fpkg6%2Fdepotd/1.0.0%2C5.11-0.3.3%3A20260923T102100Z"
eq "build publishes and archives the tagged FMRI" "$(fakebuild "${build}")" "IPS_FMRI=${want}"
dist_p5p="${fb}/dist/pkg6depotd-1.0.0-rc.3-illumos-x86_64.p5p"
eq "the archive in dist stores the manifest under the full version" \
  "$(tar -tf "${dist_p5p}" | grep -E '/pkg/[^/]+/[^/]+$')" "${want_path}"
eq "  ... declaring the full FMRI" \
  "$("${tar_cmd}" -xOf "${dist_p5p}" "${want_path}" | grep '^set name=pkg\.fmri ')" "set name=pkg.fmri value=${want}"
eq "fake pkgrecv --newest drops the build release like pkg5" \
  "$(PATH="${fb}/bin:${PATH}" pkgrecv -s "${dist_p5p}" --newest)" \
  "pkg://openindiana.org/package/pkg6/depotd@1.0.0-0.3.3:20260923T102100Z"
# OpenIndiana's developer/pkgtree owns usr/bin/pkgtree; delivering it too made
# pkg install refuse the RC2 package. The binary exists in the fixture's
# release dir (and in the tarball), yet the package must not stage it.
check "fixture: the release dir has a pkgtree binary" test -f "${fb}/co/target/release/pkgtree"
check_not "build stages no usr/bin/pkgtree into the proto" test -e "${fb}/co/target/ips/proto/usr/bin/pkgtree"
check "build stages usr/bin/pkg6repo into the proto" test -f "${fb}/co/target/ips/proto/usr/bin/pkg6repo"
check "build stages usr/bin/pkg6recv into the proto" test -f "${fb}/co/target/ips/proto/usr/bin/pkg6recv"
check_not "build never trusts pkgrecv --newest as the full FMRI" grep -q '\[ "${got}" = "${IPS_FMRI}" \]' "${build}"
mkdir -p "${fb}/noallow"
cp "${scripts}/release-lib.sh" "${fb}/noallow/"
sed 's/pkgsend -D allow-timestamp /pkgsend /' "${build}" >"${fb}/noallow/build-ips-package.sh"
check_not "fixture: the variant really drops -D allow-timestamp" grep -q "pkgsend -D allow-timestamp" "${fb}/noallow/build-ips-package.sh"
check_not "build without -D allow-timestamp fails" fakebuild "${fb}/noallow/build-ips-package.sh"
fails_closed "scratch repository's publication-time FMRI" ':20991231T235959Z'
check_not "scratch repository under another build release fails the build" \
  fakebuild "${build}" FAKE_PUBLISH_FMRI="${want/,5.11-/,5.12-}"
fails_closed "scratch repository's manifest path" 'the scratch repository holds the manifests'
check_not "archive under another build release fails the build" \
  fakebuild "${build}" FAKE_ARCHIVE_FMRI="${want/,5.11-/,5.12-}"
fails_closed "p5p archive's manifest path" 'the p5p archive holds the manifests'
check_not "archive under another timestamp fails the build" \
  fakebuild "${build}" FAKE_ARCHIVE_FMRI="${want%:*}:20260923T112700Z"
fails_closed "p5p archive's timestamp" ':20260923T112700Z'
check_not "archive with a second package fails the build" \
  fakebuild "${build}" FAKE_ARCHIVE_EXTRA="pkg://openindiana.org/package/other@1.0,5.11-0:20260923T102100Z"
fails_closed "p5p archive's extra package" 'package/other'
check_not "archive manifest declaring another build release fails the build" \
  fakebuild "${build}" FAKE_ARCHIVE_DECLARES="${want/,5.11-/,5.12-}"
fails_closed "p5p archive manifest's pkg.fmri" 'the p5p archive manifest declares'

# --- uninstall check is scoped to the repository --------------------------------
check "uninstall test searches lost+found for repository paths" \
  grep -qFe "-path '*var/pkg6depotd*'" "${scripts}/test-ips-package.sh"
check "uninstall test inspects /var/pkg/lost+found" \
  grep -qx 'lost_found=/var/pkg/lost+found' "${scripts}/test-ips-package.sh"
# repo_salvage runs under set -euo pipefail. A fresh image has no lost+found
# (pkg(1) creates it on first salvage): that is an empty result, while a
# lost+found that cannot be searched must still fail.
salvage_fn=$(sed -n '/^repo_salvage() {$/,/^}$/p' "${scripts}/test-ips-package.sh")
salvage() { # salvage LOST_FOUND
  bash -c 'set -euo pipefail; sudo() { "$@"; }; lost_found=$1; eval "$2"; out=$(repo_salvage); printf "%s" "${out}"' \
    _ "$1" "${salvage_fn}"
}
check "repo_salvage is defined in the live install test" test -n "${salvage_fn}"
eq "absent lost+found salvages nothing" "$(salvage "${tmp}/no-lost+found" 2>&1)" ""
mkdir -p "${tmp}/lf/var/pkg6depotd/repo" "${tmp}/lf/etc/pkg6depotd"
eq "repository content in lost+found is listed" "$(salvage "${tmp}/lf")" \
  "$(printf '%s\n' "${tmp}/lf/var/pkg6depotd" "${tmp}/lf/var/pkg6depotd/repo")"
if [ "$(id -u)" != 0 ]; then
  mkdir -p "${tmp}/lf-locked/x"
  chmod 000 "${tmp}/lf-locked/x"
  check_not "unreadable lost+found is an error, not an empty result" salvage "${tmp}/lf-locked"
  chmod 700 "${tmp}/lf-locked/x"
fi
check_not "uninstall test does not forbid salvaging the edited config" \
  grep -q 'ls /var/pkg/lost+found' "${scripts}/test-ips-package.sh"
check "a failed pkg uninstall is reported through fail()" \
  grep -qxF 'sudo pkg uninstall -v "${IPS_PKG_NAME}" || fail "pkg uninstall of ${IPS_PKG_NAME} failed"' \
  "${scripts}/test-ips-package.sh"

# --- master restarter checkpoints -------------------------------------------------
# RC9: svcadm disable -s failed on uninstall because the master restarter had
# lost its restarter property group. The live test records at which stage.
tip_s="${scripts}/test-ips-package.sh"
rline() { grep -nxF -- "$1" "${tip_s}" | head -1 | cut -d: -f1; }
r_install=$(rline 'check_restarter "before package install"')
r_installed=$(rline 'check_restarter "after package install"')
r_config=$(rline 'check_restarter "after the missing/invalid config exercise"')
r_smoke=$(rline 'check_restarter "after the pkg5 client smoke"')
r_upgrade=$(rline 'check_restarter "after upgrade, before uninstall"')
r_uninstall=$(rline 'sudo pkg uninstall -v "${IPS_PKG_NAME}" || fail "pkg uninstall of ${IPS_PKG_NAME} failed"')
check "restarter checkpoints run in lifecycle order before the uninstall" test -n "${r_install}" \
  -a -n "${r_installed}" -a -n "${r_config}" -a -n "${r_smoke}" -a -n "${r_upgrade}" -a -n "${r_uninstall}" \
  -a "${r_install:-0}" -lt "$(rline "sudo pkg install -v -g \"\${p5p}\" \"\${IPS_PKG_NAME}\"")" \
  -a "${r_installed:-0}" -lt "${r_config:-0}" \
  -a "$(rline 'echo "=== invalid (readable) config also fails closed in the daemon"')" -lt "${r_config:-0}" \
  -a "${r_config:-0}" -lt "$(rline 'echo "=== pkg5 client smoke: native pkg(1) and pkgrecv against the running depot"')" \
  -a "$(rline 'echo "pkg5 client smoke passed"')" -lt "${r_smoke:-0}" \
  -a "$(rline 'echo "=== upgrade: operator config must survive"')" -lt "${r_upgrade:-0}" \
  -a "${r_upgrade:-0}" -lt "${r_uninstall:-0}"
check "dump() includes the master restarter diagnostics" \
  grep -qx '  restarter_dump' <(sed -n '/^dump() {$/,/^}$/p' "${tip_s}")
restarter_fns=$(sed -n '/^master_restarter=/p; /^check_restarter() {$/,/^}$/p' "${tip_s}")
check "check_restarter is defined in the live install test" grep -q '^check_restarter() {$' <(echo "${restarter_fns}")
restarter() { # restarter SVCPROP_OUT SVCPROP_RC -> run check_restarter with a fake svcprop
  FAKE_OUT="$1" FAKE_RC="$2" bash -c 'set -euo pipefail
    svcprop() {
      [ "$*" = "-p restarter/state svc:/system/svc/restarter:default" ] || { echo "unexpected svcprop $*"; return 99; }
      printf "%s\n" "${FAKE_OUT}"; return "${FAKE_RC}"
    }
    fail() { echo "FAIL: $*"; exit 1; }
    eval "$1"; check_restarter "at test stage"' _ "${restarter_fns}"
}
check "online master restarter passes" restarter online 0
case "$(restarter online 0)" in "master restarter online at test stage ("*) ok ;; *) bad "online restarter message" ;; esac
check_not "missing restarter group fails" restarter "svcprop: Couldn't find property group \`restarter'" 1
eq "missing restarter group names the stage and the svcprop error" "$(restarter "no pg" 1 || true)" \
  "FAIL: master restarter unavailable at test stage: svcprop -p restarter/state svc:/system/svc/restarter:default: no pg"
eq "non-online restarter names the stage and the state" "$(restarter maintenance 0 || true)" \
  "FAIL: master restarter is 'maintenance' at test stage, expected online"

# --- packaging files agree with each other ----------------------------------
p5m="${root}/packaging/ips/pkg6depotd.p5m"
xml="${root}/packaging/ips/pkg6depotd.xml"
method="${root}/packaging/ips/svc-pkg6depotd"
build="${scripts}/build-ips-package.sh"
for m in $(grep -v '^#' "${p5m}" | grep -o '\$([A-Z_]*)' | sort -u | tr -d '$()'); do
  check "macro ${m} is supplied by build-ips-package.sh" grep -q -- "-D \"${m}=" "${build}"
done
# p5m_actions: one action per line (continuations joined).
p5m_actions() { awk '{ if (sub(/\\$/, "")) { buf = buf $0; next } print buf $0; buf = "" }' "${p5m}"; }
has_action() { p5m_actions | grep -Eq "$1"; }
check "config is preserve=renamenew" has_action '^file path=etc/pkg6depotd/pkg6depotd\.kdl .*preserve=renamenew'
check "config readable by pkg6srv only" has_action '^file path=etc/pkg6depotd/pkg6depotd\.kdl .*group=pkg6srv mode=0640'
check "depot restarts on binary change" has_action '^file path=usr/lib/pkg6depotd .*restart_fmri=svc:/application/pkg/pkg6depotd:default'
check "service disabled before removal" has_action '^file path=lib/svc/manifest/.*disable_fmri=svc:/application/pkg/pkg6depotd:default'
check "manifest-import on SMF manifest change" has_action '^file path=lib/svc/manifest/.*restart_fmri=svc:/system/manifest-import:default'
check "fixed uid" has_action '^user .*uid=94'
check_not "nothing delivered where repositories live (uninstall would salvage them)" has_action 'path=var/pkg6depotd'
check "default repository root is /var/pkg6depotd/repo" grep -q '^    root "/var/pkg6depotd/repo"$' "${root}/packaging/ips/pkg6depotd.kdl"
check "fixed gid" has_action '^group .*gid=94'
conf_path=/etc/pkg6depotd/pkg6depotd.kdl
check "SMF default config path" grep -q "value='${conf_path}'" "${xml}"
check "method default config path" grep -q "DEFAULT_CONFIG=${conf_path}" "${method}"
check "SMF runs as pkg6srv" grep -q "user='pkg6srv' group='pkg6srv'" "${xml}"
check "p5m creates pkg6srv" has_action '^user .*username=pkg6srv'
check "SMF uses the packaged method" grep -q "exec='/lib/svc/method/svc-pkg6depotd start'" "${xml}"
check "method execs the packaged daemon" grep -q '^DEPOTD=/usr/lib/pkg6depotd$' "${method}"
check "instance disabled by default" grep -q "<create_default_instance enabled='false' />" "${xml}"
check "child duration" grep -q "name='duration' type='astring' value='child'" "${xml}"
# duration=child reports online once the method execs the daemon, before it
# parses the file: the method must run config-test first and map a rejected
# file to SMF_EXIT_ERR_CONFIG (maintenance). Run it against stubs.
smf="${tmp}/smf"
mkdir -p "${smf}"
printf '%s\n' 'SMF_EXIT_ERR_FATAL=95' 'SMF_EXIT_ERR_CONFIG=96' >"${smf}/smf_include.sh"
printf '%s\n' '#!/bin/sh' 'echo "$*" >>"${STUB_LOG}"' \
  'case "$*" in *" config-test") [ -z "${STUB_BAD}" ] || { echo "parse error" >&2; exit 1; } ;; esac' \
  'echo "ran $*"' >"${smf}/depotd"
printf '%s\n' '#!/bin/sh' 'echo "${STUB_CONFIG}"' >"${smf}/svcprop"
chmod +x "${smf}/depotd" "${smf}/svcprop"
sed -e "s|/lib/svc/share/smf_include.sh|${smf}/smf_include.sh|" \
  -e "s|^DEPOTD=/usr/lib/pkg6depotd$|DEPOTD=${smf}/depotd|" \
  -e "s|/usr/bin/svcprop|${smf}/svcprop|" "${method}" >"${smf}/method"
: >"${smf}/good.kdl"
run_method() { # run_method CONFIG BAD -> exit status; STUB_LOG gets daemon calls
  : >"${smf}/calls"
  set +e
  STUB_LOG="${smf}/calls" STUB_CONFIG="$1" STUB_BAD="$2" SMF_FMRI=svc:/test \
    sh "${smf}/method" start >"${smf}/out" 2>&1
  local rc=$?
  set -e
  echo "${rc}"
}
eq "method: valid config execs start" "$(run_method "${smf}/good.kdl" '')" 0
eq "method: valid config is tested, then started" "$(cat "${smf}/calls")" \
  "--config ${smf}/good.kdl config-test
--config ${smf}/good.kdl start"
eq "method: invalid config exits SMF_EXIT_ERR_CONFIG" "$(run_method "${smf}/good.kdl" 1)" 96
eq "method: invalid config never starts the daemon" "$(cat "${smf}/calls")" \
  "--config ${smf}/good.kdl config-test"
check "method: invalid config logs the daemon's diagnostic" grep -q 'parse error' "${smf}/out"
check "method: invalid config says how to recover" grep -q 'svcadm clear svc:/test' "${smf}/out"
eq "method: missing config exits SMF_EXIT_ERR_CONFIG" "$(run_method "${smf}/missing.kdl" '')" 96
eq "method: missing config runs nothing" "$(cat "${smf}/calls")" ""
tools=$(sed -n 's/^for b in \(.*\); do$/\1/p' "${build}")
eq "build stages the operator tools" "${tools}" "pkg6repo pkg6recv"
check_not "p5m delivers nothing at usr/bin/pkgtree (developer/pkgtree owns it)" has_action 'path=usr/bin/pkgtree( |$)'
check_not "p5m description does not advertise pkgtree" has_action '^set name=pkg\.description .*pkgtree'
check "release tarballs still ship pkgtree" test "${RELEASE_BINARIES}" = "pkg6depotd pkg6repo pkg6recv pkgtree"
tip_tools=$(sed -n 's/^for b in \(.*\); do$/\1/p' "${scripts}/test-ips-package.sh")
eq "live install test expects the packaged tools" "${tip_tools}" "pkg6repo pkg6recv"
check_not "live install test never expects usr/bin/pkgtree" grep -q 'usr/bin/pkgtree' "${scripts}/test-ips-package.sh"
for f in usr/lib/pkg6depotd usr/bin/pkg6repo usr/bin/pkg6recv \
  lib/svc/method/svc-pkg6depotd lib/svc/manifest/application/pkg/pkg6depotd.xml \
  etc/pkg6depotd/pkg6depotd.kdl usr/share/man/man8/pkg6depotd.8 \
  usr/share/man/man1/pkg6repo.1 usr/share/man/man5/pkg6-policy.5 \
  usr/share/man/man8/pkg6repo-retire.8; do
  check "p5m delivers ${f}" has_action "^file path=${f}( |\$)"
  case "${f}" in
    usr/bin/*) ;;
    *) check "build stages ${f}" grep -Eq " ${f}\$" "${build}" ;;
  esac
done
# Manual pages: the p5m and the build agree on the set, every staged source
# exists, and each page's .Dt names the file it is installed as.
p5m_man=$(p5m_actions | sed -n 's/^file path=\(usr\/share\/man\/[^ ]*\) .*/\1/p' | sort)
build_man=$(sed -n 's/^stage "\([^"]*\)" \(usr\/share\/man\/[^ ]*\)$/\2/p' "${build}" | sort)
eq "p5m and build deliver the same manual pages" "${p5m_man}" "${build_man}"
check "four manual pages are packaged" test "$(printf '%s\n' ${p5m_man} | wc -l | tr -d ' ')" = 4
while read -r src dest; do
  src=$(eval "printf '%s' ${src}")
  if [ ! -f "${src}" ]; then bad "staged manual page source missing: ${src}"; continue; fi
  name=$(basename "${dest}")
  sect="${name##*.}"
  name="${name%.*}"
  want=".Dt $(printf '%s' "${name}" | tr '[:lower:]' '[:upper:]') ${sect}"
  check "${dest} is installed in its own section" test "$(dirname "${dest}")" = "usr/share/man/man${sect}"
  check "$(basename "${src}") .Dt matches ${dest}" grep -qx -- "${want}" "${src}"
  if command -v mandoc >/dev/null 2>&1; then
    check "$(basename "${src}") passes mandoc -Tlint" mandoc -Tlint -W warning "${src}"
  fi
done <<EOF_MAN
$(sed -n 's/^stage \("[^"]*"\) \(usr\/share\/man\/[^ ]*\)$/\1 \2/p' "${build}")
EOF_MAN
# The live install test renders exactly the pages the package delivers.
tip_man=$(sed -n 's/^man_pages="\(.*\)"$/\1/p' "${scripts}/test-ips-package.sh" | tr ' ' '\n' | sort)
eq "live test checks every packaged manual page" "${tip_man}" "$(printf '%s\n' ${p5m_man} | sed 's|^usr/share/man/||')"
# Mutable repository data is operator data: nothing the depot or pkg6repo
# writes inside a repository (generations, policy, retirement state, access
# records) may be delivered, or uninstall would salvage it.
check_not "no repository policy delivered" has_action 'path=[^ ]*policy\.kdl'
check_not "no retention access records delivered" has_action 'path=[^ ]*retention'
check_not "no generations delivered" has_action 'path=[^ ]*generations'
check_not "nothing delivered under var/" has_action '^(file|dir|link|hardlink) path=var/'
check "method is valid sh" sh -n "${method}"
if command -v xmllint >/dev/null 2>&1; then
  check "SMF manifest is well-formed" xmllint --noout "${xml}"
fi

# --- SMF property scope: config lives on the service ---------------------------
# Static only; nothing here touches SMF. pkg6depotd.xml defines the config
# property group on the service and uses create_default_instance, so the
# default instance has no config group of its own and
# `svccfg -s <instance> setprop config/...` fails on illumos.
if command -v xmllint >/dev/null 2>&1; then
  eq "config property group is defined on the service" \
    "$(xmllint --xpath "count(/service_bundle/service/property_group[@name='config'])" "${xml}")" 1
  eq "no instance-level property groups in the manifest" \
    "$(xmllint --xpath "count(//instance)" "${xml}")" 0
else
  check_not "no instance elements in the manifest (grep fallback)" grep -q '<instance' "${xml}"
fi
# svccfg_config_targets FILE: print "target" for every svccfg setprop of a
# config/ property in FILE, one per line.
svccfg_config_targets() {
  sed -n 's/.*svccfg -s \([^ ]*\) setprop config\/.*/\1/p' "$1"
}
tip="${scripts}/test-ips-package.sh"
for f in "${tip}" "${root}/packaging/ips/pkg6depotd.kdl" "${root}/packaging/ips/pkg6depotd.8" \
  "${root}/packaging/ips/README.md" "${root}/docs/release-process.md"; do
  for target in $(svccfg_config_targets "${f}"); do
    case "${target}" in
      pkg6depotd | svc:/application/pkg/pkg6depotd | '"${svc_service}"') ok ;;
      *:default | '"${svc}"')
        bad "$(basename "${f}"): svccfg setprop config/ on the default instance (${target}); target the service" ;;
      *)
        # Any other instance must create its own config group first.
        if grep -qF "svccfg -s ${target} addpg config application" "${f}"; then ok
        else bad "$(basename "${f}"): ${target} sets config/ without addpg config"; fi ;;
    esac
  done
done
eq "test sets config/file only through set_config_file" \
  "$(svccfg_config_targets "${tip}")" '"${svc_service}"'
eq "test switches config/file five times (fail-closed checks, pkg5 smoke)" "$(grep -c '^set_config_file ' "${tip}")" 5
check "set_config_file refreshes the instance" grep -q '^  smf svcadm refresh "${svc}"$' "${tip}"
check "set_config_file reads the value back through the instance" \
  grep -q 'svcprop -p config/file "${svc}"' "${tip}"
check_not "no bare sudo svcadm/svccfg (failures go through smf/fail diagnostics)" \
  grep -qE '^[[:space:]]*sudo (svcadm|svccfg) ' "${tip}"
check "start method reads properties through the composed instance view" \
  grep -q 'svcprop -p "$1" "$SMF_FMRI"' "${method}"

# --- pkg5 client smoke: comparison helpers ---------------------------------------
# shellcheck source=.forgejo/scripts/pkg5-smoke-lib.sh
. "${scripts}/pkg5-smoke-lib.sh"
# mkp5p DIR PUB STEM VER BODY PAYLOAD: an unpacked p5p with one manifest and
# one gzip payload, laid out like pkg5 writes it.
mkp5p() {
  mkdir -p "$1/publisher/$2/pkg/$3" "$1/publisher/$2/file/ab"
  printf '%s\n' "$5" >"$1/publisher/$2/pkg/$3/$4"
  printf '%s\n' "$6" | gzip -1 >"$1/publisher/$2/file/ab/ab01"
  printf 'x' >"$1/pkg5.repository"
}
sm="${tmp}/smoke"
mkdir -p "${sm}"
mkp5p "${sm}/rel" p package%2Fpkg6%2Fdepotd 1.0%2C5.11-0.1.1%3A20260923T000000Z "set name=pkg.fmri value=a" payload-a
mkdir -p "${sm}/dum/publisher/p/pkg/pkg6-release-test%2Fpkg5-smoke" "${sm}/dum/publisher/p/file/cd"
printf 'set name=pkg.fmri value=b\n' >"${sm}/dum/publisher/p/pkg/pkg6-release-test%2Fpkg5-smoke/1.0%3AZ"
printf 'hello\n' | gzip >"${sm}/dum/publisher/p/file/cd/cd02"
fresh_got() { # the depot copy: same bytes, payloads recompressed differently
  rm -rf "${sm}/got"
  cp -R "${sm}/rel" "${sm}/got"
  cp -R "${sm}/dum/publisher/p/pkg/pkg6-release-test%2Fpkg5-smoke" "${sm}/got/publisher/p/pkg/"
  mkdir -p "${sm}/got/publisher/p/file/cd"
  printf 'hello\n' | gzip -9 >"${sm}/got/publisher/p/file/cd/cd02"
  printf 'payload-a\n' | gzip -9 >"${sm}/got/publisher/p/file/ab/ab01"
}
fresh_got
check "smoke compare: identical content, different compression passes" \
  pkg5_smoke_compare "${sm}/got" "${sm}/rel" "${sm}/dum"
printf 'set name=pkg.fmri value=a \n' >"${sm}/got/publisher/p/pkg/package%2Fpkg6%2Fdepotd/1.0%2C5.11-0.1.1%3A20260923T000000Z"
check_not "smoke compare: a manifest with other bytes fails" pkg5_smoke_compare "${sm}/got" "${sm}/rel" "${sm}/dum"
fresh_got
rm "${sm}/got/publisher/p/pkg/pkg6-release-test%2Fpkg5-smoke/1.0%3AZ"
check_not "smoke compare: a missing manifest fails" pkg5_smoke_compare "${sm}/got" "${sm}/rel" "${sm}/dum"
fresh_got
printf 'hellO\n' | gzip >"${sm}/got/publisher/p/file/cd/cd02"
check_not "smoke compare: a payload with other content fails" pkg5_smoke_compare "${sm}/got" "${sm}/rel" "${sm}/dum"
fresh_got
rm "${sm}/got/publisher/p/file/ab/ab01"
check_not "smoke compare: a missing payload (no /file/ route) fails" pkg5_smoke_compare "${sm}/got" "${sm}/rel" "${sm}/dum"
fresh_got
printf 'payload-a\n' >"${sm}/got/publisher/p/file/ab/ab01"
check_not "smoke compare: an uncompressed payload fails" pkg5_smoke_compare "${sm}/got" "${sm}/rel" "${sm}/dum"
fresh_got
mkdir -p "${sm}/got/publisher/p/pkg/extra" && printf 'x\n' >"${sm}/got/publisher/p/pkg/extra/1"
check_not "smoke compare: an extra manifest fails" pkg5_smoke_compare "${sm}/got" "${sm}/rel" "${sm}/dum"
fresh_got
check_not "smoke compare: an empty depot copy fails" pkg5_smoke_compare "${sm}/nothing" "${sm}/rel" "${sm}/dum"
mkdir -p "${sm}/empty/publisher/p/pkg"
check_not "smoke compare: an empty source is never vacuous success" pkg5_smoke_compare "${sm}/empty" "${sm}/empty"
check_not "smoke compare: no sources fails" pkg5_smoke_compare "${sm}/got"
(cd "${sm}/got" && tar -cf "${sm}/got.p5p" publisher pkg5.repository)
check "smoke unpack: a tar archive unpacks" pkg5_smoke_unpack "${sm}/got.p5p" "${sm}/unpacked"
check "smoke unpack: unpacked copy compares equal" pkg5_smoke_compare "${sm}/unpacked" "${sm}/rel" "${sm}/dum"
check_not "smoke unpack: refuses an existing directory" pkg5_smoke_unpack "${sm}/got.p5p" "${sm}/unpacked"
t0=$(pkg5_smoke_tree "${sm}/got")
check "smoke tree: lists files with checksums" grep -q '^f ./publisher/p/file/ab/ab01 [0-9]' <(echo "${t0}")
check "smoke tree: lists directories" grep -qx 'd ./publisher/p/pkg' <(echo "${t0}")
eq "smoke tree: an untouched tree snapshots equal" "$(pkg5_smoke_tree "${sm}/got")" "${t0}"
printf 'y' >"${sm}/got/pkg5.repository"
check_not "smoke tree: a same-size content change is visible" test "$(pkg5_smoke_tree "${sm}/got")" = "${t0}"
fresh_got
mkdir "${sm}/got/new"
check_not "smoke tree: a new empty directory is visible" test "$(pkg5_smoke_tree "${sm}/got")" = "${t0}"
fresh_got
ln -s ab01 "${sm}/got/publisher/p/file/ab/link"
check_not "smoke tree: a new symlink is visible" test "$(pkg5_smoke_tree "${sm}/got")" = "${t0}"
fresh_got
check_not "smoke tree: a missing directory fails" pkg5_smoke_tree "${sm}/nothing"

# --- pkg5 client smoke: wiring in test-ips-package.sh ---------------------------
# Static only: the smoke needs a live OpenIndiana image and runs in tag CI.
check "test-ips-package.sh is valid bash" bash -n "${tip}"
check "pkg5-smoke-lib.sh is valid bash" bash -n "${scripts}/pkg5-smoke-lib.sh"
has_line() { grep -qF -- "$1" "${tip}"; }
line_of() { grep -nF -- "$1" "${tip}" | head -1 | cut -d: -f1; }
check "smoke imports with the installed pkg6repo import-pkg5" has_line '/usr/bin/pkg6repo import-pkg5 -s ${smoke}/src/${a}.p5p -d ${smoke}/depot/repo'
check "smoke re-imports the same archive (idempotent retry)" has_line '/usr/bin/pkg6repo import-pkg5 -s ${smoke}/src/release.p5p -d ${smoke}/depot/repo 2>&1'
check "re-import must report the archive as already imported" has_line "grep -qF 'archive already imported'"
check "re-import snapshots the repository before and after" \
  test "$(grep -c '^tree_[a-z]*=$(repo_tree) || fail' "${tip}")" = 2
check "re-import rejects an empty snapshot" has_line "grep -q '^f ' <<<\"\${tree_before}\""
check "re-import must leave the repository unchanged" has_line '[ "${tree_after}" = "${tree_before}" ]'
reimport=$(line_of '|| fail "re-importing the same archive failed')
check "snapshots bracket the re-import" test -n "${reimport}" \
  -a "$(line_of 'tree_before=$(repo_tree)')" -lt "${reimport:-0}" \
  -a "${reimport:-0}" -lt "$(line_of 'tree_after=$(repo_tree)')"
check "smoke reuses the release p5p" has_line 'cp "${p5p}" "${smoke}/src/release.p5p"'
check "smoke builds a dependency-free dummy with pkgsend" has_line 'p5 pkgsend -s "${smoke}/dummy-repo" publish'
dummy_p5m=$(sed -n '/^cat >"${smoke}\/dummy.p5m" <<EOF$/,/^EOF$/p' "${tip}")
check "dummy manifest heredoc is found (keeps the next check honest)" \
  grep -qx 'set name=pkg.fmri value=${dummy}@1.0,5.11-0' <(echo "${dummy_p5m}")
check_not "dummy package declares no dependencies" grep -q '^depend ' <(echo "${dummy_p5m}")
check "smoke has a negative control against the operator repository" \
  has_line 'fail "pkg(1) refreshed ${pub} from a depot that does not serve it"'
check "negative control requires its image-create to succeed on its own" \
  grep -qx '  || fail "pkg image-create of the negative-control image failed"' "${tip}"
check "negative control expects only the refresh to fail" \
  grep -qx 'if sp5 pkg -R "${smoke}/image-neg" refresh --full; then' "${tip}"
check "smoke creates a scratch image without an implicit refresh" \
  has_line 'sp5 pkg image-create -F --no-refresh -p "${pub}=${url}/" "${img}"'
check "smoke reads the catalog with pkg refresh --full" has_line 'sp5 pkg -R "${img}" refresh --full'
check "sp5 passes no_proxy through sudo" \
  has_line 'sp5() { sudo env TMPDIR="${smoke}/tmp" no_proxy=127.0.0.1 NO_PROXY=127.0.0.1 "$@"; }'
check "p5 bypasses proxies for the loopback depot" \
  has_line 'p5() { TMPDIR="${smoke}/tmp" no_proxy=127.0.0.1 NO_PROXY=127.0.0.1 "$@"; }'
smoke_body=$(sed -n '/^echo "=== pkg5 client smoke/,/^echo "pkg5 client smoke passed"$/p' "${tip}")
check "smoke body is found" grep -q 'pkg5 client smoke passed' <(echo "${smoke_body}")
check_not "smoke runs no pkg(1) through bare sudo (no_proxy would be lost)" grep -q 'sudo pkg ' <(echo "${smoke_body}")
smoke_code=$(echo "${smoke_body}" | grep -v '^[[:space:]]*#' | grep -v '^echo ')
check "smoke calls pkgrecv" grep -q 'pkgrecv -' <(echo "${smoke_code}")
eq "every smoke pkgrecv call is wrapped by p5 (no_proxy)" \
  "$(echo "${smoke_code}" | grep -oE 'pkgrecv -' | grep -c .)" "$(echo "${smoke_code}" | grep -oE '(^|[^[:alnum:]_])p5 pkgrecv -' | grep -c .)"
check "smoke compares the catalog with the source FMRIs" has_line '[ "${got}" = "${expected}" ] || fail "pkg(1) catalog lists'
check "smoke fetches the manifest with pkg info -r" has_line 'sp5 pkg -R "${img}" info -r "${rel_fmri}"'
check "smoke pulls from the depot with pkgrecv into an archive" \
  has_line 'p5 pkgrecv -s "${url}/" -c "${smoke}/pkgrecv-cache" -a -d "${smoke}/received.p5p"'
check "smoke compares the pkgrecv copy with both sources" \
  has_line 'pkg5_smoke_compare "${smoke}/unpacked-received" "${smoke}/unpacked-release" "${smoke}/unpacked-dummy"'
check "smoke installs the dummy from the depot" has_line 'sp5 pkg -R "${img}" install -v "${dummy_fmri}"'
check "smoke verifies the installed dummy" has_line 'sp5 pkg -R "${img}" verify -v "${dummy}"'
# Confinement: pkg(1) only ever runs against the scratch images, nothing is
# imported into the operator repository, and the scratch dir is removed.
eq "pkg -R only targets scratch images" \
  "$(grep -oE 'pkg -R "[^"]*"' "${tip}" | sort -u | tr '\n' ' ')" 'pkg -R "${img}" pkg -R "${smoke}/image-neg" '
check_not "nothing is imported into the operator repository" grep -qE 'import-pkg5.*-d /var/pkg6depotd' "${tip}"
check "smoke scratch dir is under /var/tmp" has_line 'smoke=$(mktemp -d /var/tmp/pkg6-pkg5-smoke.XXXXXX)'
check "smoke cleans up on failure" has_line 'trap cleanup_smoke EXIT'
check "smoke cleans up on success" has_line '[ ! -e "${smoke}" ] || fail "scratch directory ${smoke} was not removed"'
smoke_start=$(line_of '=== pkg5 client smoke')
switch_back=$(awk -v s="${smoke_start}" 'NR > s && $0 == "set_config_file \"${conf}\"" { print NR; exit }' "${tip}")
upgrade=$(line_of '=== upgrade: operator config must survive')
check "smoke switches back to the operator config before the upgrade" \
  test -n "${switch_back}" -a "${smoke_start:-0}" -lt "${switch_back:-0}" -a "${switch_back:-0}" -lt "${upgrade:-0}"
eq "after switching back the depot restarts and no longer serves the smoke publisher" \
  "$(sed -n "$((switch_back + 1)),$((switch_back + 2))p" "${tip}")" \
  "$(printf '%s\n%s' depot_restart '! serves_pub || fail "depot still serves ${pub} after switching back to ${conf}"')"

# --- unprivileged pkg5 smoke: test-pkg5-unprivileged.sh ---------------------------
# Its argument and preflight errors, which fail before any scratch directory
# exists. The pkg5 run itself needs OpenIndiana and is done by hand on a build
# host (docs/release-process.md). The lint below only catches obvious
# regressions in confinement; it does not prove the script is safe.
tpu="${scripts}/test-pkg5-unprivileged.sh"
check "test-pkg5-unprivileged.sh is valid bash" bash -n "${tpu}"
check "unprivileged smoke is executable" test -x "${tpu}"
tpu_code=$(grep -v '^[[:space:]]*#' "${tpu}")
check_not "unprivileged smoke uses no sudo, su, pfexec or SMF" \
  grep -qE '(^|[^[:alnum:]_-])(sudo|su|pfexec|svccfg|svcadm)( |$)' <(echo "${tpu_code}")
check_not "unprivileged smoke never runs the installed /usr/bin/pkg6*" grep -q '/usr/bin/pkg6' <(echo "${tpu_code}")
eq "unprivileged smoke removes only its scratch directory" \
  "$(echo "${tpu_code}" | grep -oE 'rm -[a-z]+ .*' | sort -u)" 'rm -rf -- "${smoke}"'
eq "unprivileged smoke signals only its own depot" \
  "$(echo "${tpu_code}" | grep -oE 'kill( -[0-9A-Z]+)? "[^"]*"' | sed -E 's/kill( -[0-9A-Z]+)? //' | sort -u)" '"${depot_pid}"'
if [ "$(id -u)" != 0 ]; then
  tpu_run() { # tpu_run ENV... -- ARGS...: run the smoke, print "rc=N" and its output
    local env=()
    while [ "$1" != -- ]; do env+=("$1"); shift; done
    shift
    local out rc=0
    out=$(env "${env[@]+"${env[@]}"}" "${tpu}" "$@" 2>&1) || rc=$?
    printf 'rc=%s\n%s\n' "${rc}" "${out}"
  }
  out=$(tpu_run --)
  check "unprivileged smoke without an archive prints usage and exits 2" \
    grep -qx 'rc=2' <(echo "${out}")
  check "  ... naming the argument" grep -q 'usage: .* P5P' <(echo "${out}")
  out=$(tpu_run -- "${tmp}/absent.p5p")
  check "unprivileged smoke rejects a missing archive" grep -qx 'rc=1' <(echo "${out}")
  check "  ... and names it" grep -qF "FAIL: release archive '${tmp}/absent.p5p' does not exist" <(echo "${out}")
  : >"${tmp}/fake.p5p"
  mkdir -p "${tmp}/tpu-bin"
  printf '#!/bin/sh\nexit 0\n' >"${tmp}/tpu-bin/pkg6repo"
  cp "${tmp}/tpu-bin/pkg6repo" "${tmp}/tpu-bin/pkg6depotd"
  chmod +x "${tmp}/tpu-bin/pkg6repo" "${tmp}/tpu-bin/pkg6depotd"
  out=$(tpu_run PKG6_BIN_DIR="${tmp}/absent-bin" -- "${tmp}/fake.p5p")
  check "unprivileged smoke names a missing pkg6 binary" \
    grep -qF "FAIL: ${tmp}/absent-bin/pkg6repo is missing" <(echo "${out}")
  out=$(tpu_run PKG6_BIN_DIR="${tmp}/tpu-bin" PKG6_SMOKE_PORT=80 -- "${tmp}/fake.p5p")
  check "unprivileged smoke rejects a privileged port" \
    grep -qF 'FAIL: PKG6_SMOKE_PORT=80 is not an unprivileged port' <(echo "${out}")
  out=$(tpu_run PKG6_BIN_DIR="${tmp}/tpu-bin" PKG6_SMOKE_PORT=8x -- "${tmp}/fake.p5p")
  check "unprivileged smoke rejects a non-numeric port" \
    grep -qF "FAIL: PKG6_SMOKE_PORT='8x' is not a port number" <(echo "${out}")
  # Only where pkg5 is absent: on OpenIndiana the preflight would pass.
  if ! PATH=/usr/bin:/bin command -v pkgrecv >/dev/null 2>&1; then
    out=$(tpu_run PKG6_BIN_DIR="${tmp}/tpu-bin" PATH=/usr/bin:/bin -- "${tmp}/fake.p5p")
    check "unprivileged smoke names a missing pkg5 tool" \
      grep -qE "FAIL: required tool '[a-z]+' is not in PATH" <(echo "${out}")
  fi
fi

# --- release test inventory -----------------------------------------------------
# Synthetic `cargo nextest list --message-format json` documents.
inv() { # inv FILE SUITE=COUNT... -> write a nextest list JSON of selected tests
  local out="$1" spec suite count i json='{}'
  shift
  for spec in "$@"; do
    suite="${spec%=*}"; count="${spec#*=}"
    i=0
    while [ "${i}" -lt "${count}" ]; do
      json=$(printf '%s' "${json}" | jq --arg s "${suite}" --arg t "t${i}" \
        '.[$s].testcases[$t] = {kind: "test", ignored: false, "filter-match": {status: "matches"}}')
      i=$((i + 1))
    done
  done
  printf '%s' "${json}" | jq '{"rust-suites": .}' > "${out}"
}
full_inventory() { # the required suites at their minimums, plus the allowed ignored test
  # shellcheck disable=SC2086
  inv "$1" ${RELEASE_REQUIRED_SUITES}
  jq '.["rust-suites"]["libips::e2e_openindiana"].testcases["e2e_download_and_build_catalog_openindiana"] =
    {kind: "test", ignored: true, "filter-match": {status: "mismatch", reason: "ignored"}}' "$1" > "$1.tmp" && mv "$1.tmp" "$1"
}
full_inventory "${tmp}/inv-ok.json"
check "inventory: required suites at minimum plus allowed ignored test pass" release_check_test_inventory "${tmp}/inv-ok.json"
jq 'del(.["rust-suites"]["pkg6repo::e2e_tests"])' "${tmp}/inv-ok.json" > "${tmp}/inv-no-e2e.json"
check_not "inventory: missing pkg6repo e2e suite fails" release_check_test_inventory "${tmp}/inv-no-e2e.json"
jq 'del(.["rust-suites"]["pkg6repo::e2e_tests"].testcases.t0)' "${tmp}/inv-ok.json" > "${tmp}/inv-short.json"
check_not "inventory: a shrunken suite fails" release_check_test_inventory "${tmp}/inv-short.json"
jq '.["rust-suites"]["pkg6repo::e2e_tests"].testcases.t0["filter-match"] = {status: "mismatch", reason: "ignored"}' \
  "${tmp}/inv-ok.json" > "${tmp}/inv-ignored.json"
check_not "inventory: a newly ignored required test fails" release_check_test_inventory "${tmp}/inv-ignored.json"
jq '.["rust-suites"]["libips"].testcases.new_ignored = {kind: "test", ignored: true, "filter-match": {status: "mismatch", reason: "ignored"}}' \
  "${tmp}/inv-ok.json" > "${tmp}/inv-other-ignored.json"
check_not "inventory: any other unselected test fails" release_check_test_inventory "${tmp}/inv-other-ignored.json"
printf '{}' > "${tmp}/inv-garbage.json"
check_not "inventory: non-nextest JSON fails" release_check_test_inventory "${tmp}/inv-garbage.json"
check "inventory requires the pkg6repo e2e suite" grep -q 'pkg6repo::e2e_tests=5' "${scripts}/release-lib.sh"
for suite in libips::atomic_pkg5_import pkg6repo::import_pkg5_cli pkg6depotd::atomic_import_http_tests \
  pkg6depotd::telemetry_otlp pkg6depotd::tls_config pkg6depotd::tls_acme pkg6depotd::tls_depot_e2e \
  pkg6depotd::packaged_config pkg6depotd libips libips::retirement pkg6repo::retire_cli \
  pkg6depotd::retirement_http_tests pkg6depotd::access_lifecycle_e2e pkg6repo::cli_surface pkg6repo::manpages "pkg6repo::bin/pkg6repo" \
  pkg6depotd::cli_surface; do
  check "inventory requires the suite ${suite}" grep -qE "[\" ]${suite}=[1-9]" "${scripts}/release-lib.sh"
  jq --arg s "${suite}" 'del(.["rust-suites"][$s])' "${tmp}/inv-ok.json" > "${tmp}/inv-no-suite.json"
  check_not "inventory: missing suite ${suite} fails" release_check_test_inventory "${tmp}/inv-no-suite.json"
done

# --- e2e tests are self-contained ------------------------------------------------
e2e="${root}/pkg6repo/src/e2e_tests.rs"
check_not "e2e tests do not call the removed pkg6dev tool" grep -qE 'run_pkg6dev|"pkg6dev"|join\("pkg6dev"\)' "${e2e}"
check "e2e tests drive the Cargo-built pkg6repo" grep -q 'env!("CARGO_BIN_EXE_pkg6repo")' "${e2e}"
check_not "e2e tests use no shared /tmp fixture paths" grep -q '"/tmp/' "${e2e}"
check_not "e2e tests do not rebuild the workspace via xtask" grep -q 'xtask' "${e2e}"
check_not "e2e tests are not also compiled into the pkg6repo binary" grep -qE '^mod e2e_tests;' "${root}/pkg6repo/src/main.rs"
eq "xtask spawns commands only through its status-checking run()" \
  "$(grep -v '^[[:space:]]*//' "${root}/xtask/src/main.rs" | grep -c '\.status()')" 1
check "xtask run() fails on a non-zero exit" grep -q 'if !status.success()' "${root}/xtask/src/main.rs"

# --- workflow respects Solstice constraints ---------------------------------
wf_all="${root}/.forgejo/workflows/release.yml"
wf="${tmp}/release.yml.code"
grep -v '^ *#' "${wf_all}" > "${wf}"
check_not "no needs: (ignored by Solstice)" grep -qE '^ +needs:' "${wf}"
check_not "no uses: steps" grep -qE '^ +- +uses:' "${wf}"
check_not "no \${{ env.* }} (always empty on Solstice)" grep -q '\${{ *env\.' "${wf}"
check_not "FORGEJO_TOKEN not shadowed by an empty secret" grep -q 'FORGEJO_TOKEN: *\${{' "${wf}"
check_not "no staged e2e binaries or PKG6_TEST_BIN_DIR" grep -qE 'PKG6_TEST_BIN_DIR|/tmp/pkg6_test' "${wf}"
check_not "test runs are never filtered (-E/--skip/--exact/--ignored)" \
  grep -qE 'nextest run[^|]*( -E | --filterset| --skip| --exact| --run-ignored| -- )' "${wf}"
for w in "${root}/.forgejo/workflows/ci.yml" "${wf_all}"; do
  check "$(basename "${w}") checks the test inventory" grep -q 'release_check_test_inventory' "${w}"
done
# Every multi-line run block starts with strict mode.
awk '
  /run: \|$/ { want = 1; next }
  want { if ($1 != "set" || $2 != "-euo" || $3 != "pipefail") { print "step at line " NR " lacks set -euo pipefail"; bad = 1 }; want = 0 }
  END { exit bad }' "${wf_all}" >"${tmp}/strict.out" && ok || bad "$(cat "${tmp}/strict.out")"
# The illumos release job is suspended until its throwaway-VM gate is
# repaired; illumos assets are built by hand (docs/release-process.md).
for job in linux; do
  body=$(awk -v j="  ${job}:" '$0 == j { f = 1; next } f && /^  [a-z]/ { f = 0 } f' "${wf}")
  for gate in release_verify_commit release_verify_cargo_version test-release-scripts.sh \
    "cargo nextest list --workspace --release --locked --message-format json" release_check_test_inventory \
    "cargo nextest run --workspace --release --locked" write-provenance.sh upload-release-assets.sh; do
    case "${body}" in *"${gate}"*) ok ;; *) bad "job ${job} lacks gate ${gate}" ;; esac
  done
done

echo "release script tests: ${pass} passed, ${failed} failed"
[ "${failed}" -eq 0 ]
