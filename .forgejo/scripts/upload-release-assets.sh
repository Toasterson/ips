#!/usr/bin/env bash
# Attach one platform's assets to the Forgejo release of the tag being built.
# Used by .forgejo/workflows/release.yml.
#
# Usage: upload-release-assets.sh PLATFORM DIR
#   Uploads release_platform_assets(PLATFORM) from DIR (see release-lib.sh).
#
# Rules:
#   * The release is created as a draft, flagged prerelease for -alpha/-beta/
#     -rc tags. Two platform jobs race to create it; the loser re-reads it.
#   * Assets are immutable. Every existing asset is checked before anything
#     is uploaded: identical bytes are skipped (safe re-run), different bytes
#     or an asset that cannot be downloaded for comparison fail the job. Nothing
#     is ever deleted or replaced, and nothing is added to a non-draft release.
#     The .p5p is not byte-reproducible, so an illumos re-run after a partial
#     upload fails closed; see docs/release-process.md for the draft cleanup.
#   * Once every platform's assets are present, a pre-release is taken out of
#     draft. A stable (GA) release always stays a draft: a maintainer checks
#     provenance and publishes it by hand.
#   * RELEASE_NOTES_FILE, when set, is appended to the release body before
#     anything is uploaded, so the builder/gate statement is on the draft
#     before the last asset can publish it. Only a draft's body is changed.
#     illumos assets are built outside CI (docs/release-process.md), so they
#     are refused without such a statement, and the statement must give the
#     live-image/SMF gate result (NOT RUN or passed) on one line.
#
# Env: FORGE_BASE, REPO_OWNER, REPO_NAME, GITHUB_REF (tag), and credentials
# with release write access:
#   * RELEASE_AUTH=basic (a maintainer's machine): RELEASE_BASIC_USER and
#     RELEASE_BASIC_PASSWORD, both non-empty, for HTTP Basic. Nothing else is
#     accepted: an empty pair fails, it never falls back to a token.
#   * RELEASE_AUTH unset (CI): the Basic pair if either half is set, else
#     RELEASE_TOKEN, else FORGEJO_TOKEN.
# The Basic pair is copied into unexported shell variables and unset before
# the script starts any child process, and reaches curl only through a pipe
# (--config -), never argv or a file. Credentials are never printed.
set -euo pipefail

# Before the first child process (dirname below): take the Basic pair out of
# the environment so no child inherits it. This does not scrub this
# process's own initial environment block; see docs/release-process.md.
declare +x basic_user="${RELEASE_BASIC_USER-}" basic_password="${RELEASE_BASIC_PASSWORD-}"
unset RELEASE_BASIC_USER RELEASE_BASIC_PASSWORD

here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=release-lib.sh
. "${here}/release-lib.sh"

[ "$#" -eq 2 ] || { echo "usage: $0 PLATFORM DIR" >&2; exit 2; }
platform="$1"
dir="$2"

# The Basic pair wins over tokens so an unrelated FORGEJO_TOKEN in a
# maintainer's shell is not used by accident. RELEASE_AUTH=basic makes that
# a requirement: if the pair came out empty (a failed secret read), stop.
token=""
case "${RELEASE_AUTH:-}" in
  "") ;;
  basic)
    [ -n "${basic_user}" ] && [ -n "${basic_password}" ] \
      || release_die "RELEASE_AUTH=basic needs non-empty RELEASE_BASIC_USER and RELEASE_BASIC_PASSWORD; not falling back to a token"
    unset RELEASE_TOKEN FORGEJO_TOKEN
    ;;
  *) release_die "RELEASE_AUTH must be 'basic' or unset, not '${RELEASE_AUTH}'" ;;
esac
if [ -n "${basic_user}" ] || [ -n "${basic_password}" ]; then
  [ -n "${basic_user}" ] && [ -n "${basic_password}" ] \
    || release_die "RELEASE_BASIC_USER and RELEASE_BASIC_PASSWORD must be set together"
  case "${basic_user}" in *:*) release_die "RELEASE_BASIC_USER must not contain ':'" ;; esac
  case "${basic_user}${basic_password}" in
    *[[:cntrl:]]*) release_die "RELEASE_BASIC_USER or RELEASE_BASIC_PASSWORD contains a control character" ;;
  esac
  echo "auth: HTTP Basic from RELEASE_BASIC_USER/RELEASE_BASIC_PASSWORD"
else
  token="${RELEASE_TOKEN:-${FORGEJO_TOKEN:-}}"
  [ -n "${token}" ] || release_die "set RELEASE_BASIC_USER and RELEASE_BASIC_PASSWORD, or RELEASE_TOKEN or FORGEJO_TOKEN; cannot publish"
fi

# curl_auth [curl args...]: curl with the chosen credentials. Basic auth goes
# in as a curl config on stdin, written by the printf builtin, so neither the
# password nor its base64 form is ever in an argv or a file; curl builds the
# Authorization: Basic header itself. Config strings escape \ and ".
curl_auth() {
  if [ -n "${basic_user}" ]; then
    local cred="${basic_user}:${basic_password}"
    cred="${cred//\\/\\\\}"
    cred="${cred//\"/\\\"}"
    printf 'user = "%s"\n' "${cred}" | curl --config - "$@"
  else
    curl -H "Authorization: token ${token}" "$@"
  fi
}

# release_live_gate NOTES: the illumos live-image/SMF test does not run on
# the build host, so the notes must say what became of it. At least one line
# names live-image and SMF and gives a verdict, NOT RUN or passed; lines that
# disagree, or one line with both verdicts, are refused. Checked before any
# network call.
release_live_gate() {
  local lines verdicts
  lines=$(printf '%s\n' "$1" | grep -iE 'live[- ]image' | grep -i 'smf') || lines=""
  verdicts=$(printf '%s\n' "${lines}" | awk '
    { l = tolower($0); n = (l ~ /(^|[^[:alnum:]])not run([^[:alnum:]]|$)/)
      p = (l ~ /(^|[^[:alnum:]])passed([^[:alnum:]]|$)/)
      if (n && p) print "both"; else if (n) print "NOT RUN"; else if (p) print "passed" }' | sort -u)
  case "${verdicts}" in
    "NOT RUN" | passed) echo "illumos live-image/SMF gate: ${verdicts}" ;;
    "") release_die "RELEASE_NOTES_FILE must state the illumos live-image/SMF gate as NOT RUN or passed, on a line naming live-image and SMF" ;;
    *) release_die "RELEASE_NOTES_FILE gives conflicting live-image/SMF gate results; state exactly one of NOT RUN or passed" ;;
  esac
}

notes=""
if [ -n "${RELEASE_NOTES_FILE:-}" ]; then
  [ -s "${RELEASE_NOTES_FILE}" ] || release_die "RELEASE_NOTES_FILE ${RELEASE_NOTES_FILE} is missing or empty"
  notes=$(cat "${RELEASE_NOTES_FILE}")
  if [ "${platform}" = illumos-x86_64 ]; then release_live_gate "${notes}"; fi
elif [ "${platform}" = illumos-x86_64 ]; then
  release_die "illumos assets are built outside CI; set RELEASE_NOTES_FILE to the builder and gate statement (docs/release-process.md)"
fi

release_init
release_fetch_tag
release_verify_commit

assets=$(release_platform_assets "${platform}")
for name in ${assets}; do
  [ -f "${dir}/${name}" ] || release_die "missing build output ${dir}/${name}"
done

api="${FORGE_BASE}/api/v1/repos/${REPO_OWNER}/${REPO_NAME}"
tmp=$(mktemp -d)
trap 'rm -rf "${tmp}"' EXIT

# call METHOD URL [curl args...]: body to ${tmp}/body, prints the HTTP status.
call() {
  local method="$1" url="$2"
  shift 2
  curl_auth -sS -o "${tmp}/body" -w '%{http_code}' -X "${method}" "$@" "${url}"
}

# find_release: print the release JSON for the tag (drafts included), or
# nothing. The list endpoint is used because the by-tag lookup does not
# return drafts. Runs inside $(...), where errexit does not apply, so every
# failure returns explicitly.
find_release() {
  local page=1 status n
  while [ "${page}" -le 100 ]; do
    status=$(call GET "${api}/releases?limit=50&page=${page}") || status=000
    if [ "${status}" != 200 ]; then
      release_die "listing releases failed (HTTP ${status}): $(cat "${tmp}/body" 2>/dev/null)"
      return 1
    fi
    n=$(jq 'length' "${tmp}/body") || { release_die "release list is not JSON"; return 1; }
    if jq -e --arg t "${RELEASE_TAG}" 'any(.[]; .tag_name == $t)' "${tmp}/body" >/dev/null; then
      jq -c --arg t "${RELEASE_TAG}" '[.[] | select(.tag_name == $t)][0]' "${tmp}/body"
      return 0
    fi
    [ "${n}" -gt 0 ] || return 0
    page=$((page + 1))
  done
  release_die "more than 100 pages of releases; refusing to guess"
  return 1
}

body=$(printf 'Built from tag `%s`, commit `%s` (%s tag object `%s`).\n\nAssets are immutable. Verify them with the per-platform `SHA256SUMS-*` files; `provenance-*.json` records the tag, commit and toolchain of each build.\n' \
  "${RELEASE_TAG}" "${RELEASE_COMMIT}" "${RELEASE_TAG_TYPE}" "${RELEASE_TAG_OBJECT}")
if [ "${RELEASE_PRERELEASE}" = false ]; then
  body="${body}"$'\n''This is a stable release. It stays a draft; a maintainer publishes it after review.'
fi

release=$(find_release)
if [ -z "${release}" ]; then
  echo "creating draft release ${RELEASE_TAG} (prerelease=${RELEASE_PRERELEASE})"
  status=$(call POST "${api}/releases" -H 'Content-Type: application/json' \
    -d "$(jq -n --arg t "${RELEASE_TAG}" --arg c "${RELEASE_COMMIT}" --arg b "${body}" \
      --argjson pre "${RELEASE_PRERELEASE}" \
      '{tag_name: $t, name: $t, target_commitish: $c, body: $b, draft: true, prerelease: $pre}')") || status=000
  if [ "${status}" = 201 ]; then
    release=$(cat "${tmp}/body")
  else
    # Most likely the other platform job created it first.
    echo "create returned HTTP ${status}; looking the release up again"
    release=$(find_release)
    [ -n "${release}" ] || release_die "could not create or find release ${RELEASE_TAG}"
  fi
fi

rid=$(printf '%s' "${release}" | jq -r '.id')
is_draft=$(printf '%s' "${release}" | jq -r '.draft')
is_pre=$(printf '%s' "${release}" | jq -r '.prerelease')
echo "release ${RELEASE_TAG}: id=${rid} draft=${is_draft} prerelease=${is_pre}"
[ "${is_pre}" = "${RELEASE_PRERELEASE}" ] \
  || release_die "release ${RELEASE_TAG} has prerelease=${is_pre}, tag channel ${RELEASE_CHANNEL} requires ${RELEASE_PRERELEASE}; fix it by hand"

list_assets() {
  local status
  status=$(call GET "${api}/releases/${rid}/assets") || status=000
  if [ "${status}" != 200 ]; then
    release_die "listing assets failed (HTTP ${status}): $(cat "${tmp}/body" 2>/dev/null)"
    return 1
  fi
  cp "${tmp}/body" "${tmp}/assets.json"
}

# compare_existing NAME FILE: 0 identical, 1 different bytes, 2 the uploaded
# asset could not be downloaded (so it cannot be proven identical).
compare_existing() {
  local name="$1" file="$2" url uuid
  url=$(jq -r --arg n "${name}" '[.[] | select(.name == $n)][0].browser_download_url // empty' "${tmp}/assets.json")
  uuid=$(jq -r --arg n "${name}" '[.[] | select(.name == $n)][0].uuid // empty' "${tmp}/assets.json")
  rm -f "${tmp}/existing"
  # Draft assets may not be served at browser_download_url; try the
  # attachment URL next.
  if [ -z "${url}" ] || ! curl_auth -sSfL -o "${tmp}/existing" "${url}" 2>/dev/null; then
    if [ -z "${uuid}" ] || ! curl_auth -sSfL -o "${tmp}/existing" \
      "${FORGE_BASE}/attachments/${uuid}" 2>/dev/null; then
      return 2
    fi
  fi
  [ "$(release_sha256 "${tmp}/existing")" = "$(release_sha256 "${file}")" ] || return 1
}

cleanup_hint="If this rebuild is intended and ${RELEASE_TAG} is still a draft, delete its assets (or the draft release) in Forgejo, then re-run both platform jobs."

# Pass 1: prove every already-uploaded asset of this platform identical
# before changing anything, so a mismatching re-run uploads nothing.
list_assets
pending=""
for name in ${assets}; do
  if ! jq -e --arg n "${name}" 'any(.[]; .name == $n)' "${tmp}/assets.json" >/dev/null; then
    pending="${pending} ${name}"
    continue
  fi
  rc=0
  compare_existing "${name}" "${dir}/${name}" || rc=$?
  case "${rc}" in
    0) echo "asset ${name} already uploaded with identical content; skipping" ;;
    1) release_die "asset ${name} already exists on release ${RELEASE_TAG} with different content; assets are immutable. ${cleanup_hint}" ;;
    *) release_die "asset ${name} already exists on release ${RELEASE_TAG} but could not be downloaded to compare it; refusing to assume it is identical. ${cleanup_hint}" ;;
  esac
done

# Record the builder/gate statement on the draft before any upload, so the
# final asset can never publish a pre-release without it. Idempotent: the
# body is the generated text plus the notes, and a published release is only
# accepted if it already carries exactly that body.
if [ -n "${notes}" ]; then
  want_body="${body}"$'\n\n'"${notes}"
  have_body=$(printf '%s' "${release}" | jq -r '.body // ""')
  if [ "${have_body}" = "${want_body}" ]; then
    echo "release body already carries the notes"
  elif [ "${is_draft}" != true ]; then
    release_die "release ${RELEASE_TAG} is already published; refusing to change its body"
  else
    echo "writing release notes from ${RELEASE_NOTES_FILE} to the draft body"
    status=$(call PATCH "${api}/releases/${rid}" -H 'Content-Type: application/json' \
      -d "$(jq -cn --arg b "${want_body}" '{body: $b}')") || status=000
    [ "${status}" = 200 ] || release_die "updating the release body failed (HTTP ${status}): $(cat "${tmp}/body")"
    [ "$(jq -r '.body // ""' "${tmp}/body")" = "${want_body}" ] \
      || release_die "release body did not take the notes; nothing uploaded"
    [ "$(jq -r '.draft' "${tmp}/body")" = true ] \
      || release_die "release ${RELEASE_TAG} left draft while its body was updated; nothing uploaded"
  fi
fi

# Pass 2: upload what is missing, only into a draft.
for name in ${pending}; do
  file="${dir}/${name}"
  [ "${is_draft}" = true ] \
    || release_die "release ${RELEASE_TAG} is already published; refusing to add ${name}"
  echo "uploading ${name}"
  status=$(call POST "${api}/releases/${rid}/assets?name=${name}" -F "attachment=@${file}") || status=000
  [ "${status}" = 201 ] || release_die "uploading ${name} failed (HTTP ${status}): $(cat "${tmp}/body")"
  got=$(jq -r '.size' "${tmp}/body")
  want=$(wc -c < "${file}" | tr -d ' ')
  [ "${got}" = "${want}" ] || release_die "uploaded ${name} is ${got} bytes, expected ${want}"
done

# Finalize: publish a complete pre-release; never publish a stable release.
list_assets
missing=""
for name in $(release_expected_assets); do
  jq -e --arg n "${name}" 'any(.[]; .name == $n)' "${tmp}/assets.json" >/dev/null || missing="${missing} ${name}"
done
if [ -n "${missing}" ]; then
  echo "release ${RELEASE_TAG} stays a draft; still waiting for:${missing}"
elif [ "${RELEASE_PRERELEASE}" = true ] && [ "${is_draft}" = true ]; then
  echo "all assets present; publishing pre-release ${RELEASE_TAG}"
  status=$(call PATCH "${api}/releases/${rid}" -H 'Content-Type: application/json' \
    -d '{"draft": false, "prerelease": true}') || status=000
  [ "${status}" = 200 ] || release_die "publishing pre-release failed (HTTP ${status}): $(cat "${tmp}/body")"
elif [ "${RELEASE_PRERELEASE}" = false ]; then
  echo "all assets present; stable release ${RELEASE_TAG} left as a draft for maintainer review"
fi
echo "done"
