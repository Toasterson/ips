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
upload() { # upload STATE TAG PLATFORM DIR
  (cd "${ci}" && PATH="${tmp}/bin:${PATH}" FAKE_FORGE_STATE="$1" GITHUB_REF="refs/tags/$2" \
    FORGE_BASE=http://forge.invalid REPO_OWNER=o REPO_NAME=r FORGEJO_TOKEN=x RELEASE_TOKEN='' \
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
check_not "stable was never patched" grep -q PATCH "${st}/log"

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

# --- uninstall check is scoped to the repository --------------------------------
check "uninstall test searches lost+found for repository paths" \
  grep -qF "find /var/pkg/lost+found -path '*var/pkg6depotd*'" "${scripts}/test-ips-package.sh"
check_not "uninstall test does not forbid salvaging the edited config" \
  grep -q 'ls /var/pkg/lost+found' "${scripts}/test-ips-package.sh"

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
tools=$(sed -n 's/^for b in \(.*\); do$/\1/p' "${build}")
eq "build stages the operator tools" "${tools}" "pkg6repo pkg6recv pkgtree"
for f in usr/lib/pkg6depotd usr/bin/pkg6repo usr/bin/pkg6recv usr/bin/pkgtree \
  lib/svc/method/svc-pkg6depotd lib/svc/manifest/application/pkg/pkg6depotd.xml \
  etc/pkg6depotd/pkg6depotd.kdl usr/share/man/man8/pkg6depotd.8; do
  check "p5m delivers ${f}" has_action "^file path=${f}( |\$)"
  case "${f}" in
    usr/bin/*) ;;
    *) check "build stages ${f}" grep -Eq " ${f}\$" "${build}" ;;
  esac
done
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
eq "test switches config/file three times" "$(grep -c '^set_config_file ' "${tip}")" 3
check "set_config_file refreshes the instance" grep -q '^  smf svcadm refresh "${svc}"$' "${tip}"
check "set_config_file reads the value back through the instance" \
  grep -q 'svcprop -p config/file "${svc}"' "${tip}"
check_not "no bare sudo svcadm/svccfg (failures go through smf/fail diagnostics)" \
  grep -qE '^[[:space:]]*sudo (svcadm|svccfg) ' "${tip}"
check "start method reads properties through the composed instance view" \
  grep -q 'svcprop -p "$1" "$SMF_FMRI"' "${method}"

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
  pkg6depotd::packaged_config pkg6depotd; do
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
for job in linux illumos; do
  body=$(awk -v j="  ${job}:" '$0 == j { f = 1; next } f && /^  [a-z]/ { f = 0 } f' "${wf}")
  for gate in release_verify_commit release_verify_cargo_version test-release-scripts.sh \
    "cargo nextest list --workspace --release --locked --message-format json" release_check_test_inventory \
    "cargo nextest run --workspace --release --locked" write-provenance.sh upload-release-assets.sh; do
    case "${body}" in *"${gate}"*) ok ;; *) bad "job ${job} lacks gate ${gate}" ;; esac
  done
done

echo "release script tests: ${pass} passed, ${failed} failed"
[ "${failed}" -eq 0 ]
