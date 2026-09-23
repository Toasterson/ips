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
#
# Env: FORGE_BASE, REPO_OWNER, REPO_NAME, GITHUB_REF (tag), and RELEASE_TOKEN
# or FORGEJO_TOKEN (needs release write access).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=release-lib.sh
. "${here}/release-lib.sh"

[ "$#" -eq 2 ] || { echo "usage: $0 PLATFORM DIR" >&2; exit 2; }
platform="$1"
dir="$2"

token="${RELEASE_TOKEN:-${FORGEJO_TOKEN:-}}"
[ -n "${token}" ] || release_die "neither RELEASE_TOKEN nor FORGEJO_TOKEN is set; cannot publish"

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
  curl -sS -o "${tmp}/body" -w '%{http_code}' -X "${method}" \
    -H "Authorization: token ${token}" "$@" "${url}"
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

release=$(find_release)
if [ -z "${release}" ]; then
  echo "creating draft release ${RELEASE_TAG} (prerelease=${RELEASE_PRERELEASE})"
  body=$(printf 'Built by Solstice CI from tag `%s`, commit `%s` (%s tag object `%s`).\n\nAssets are immutable. Verify them with the per-platform `SHA256SUMS-*` files; `provenance-*.json` records the tag, commit and toolchain of each build.\n' \
    "${RELEASE_TAG}" "${RELEASE_COMMIT}" "${RELEASE_TAG_TYPE}" "${RELEASE_TAG_OBJECT}")
  if [ "${RELEASE_PRERELEASE}" = false ]; then
    body="${body}"$'\n''This is a stable release. CI leaves it as a draft; a maintainer publishes it after review.'
  fi
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
  if [ -z "${url}" ] || ! curl -sSfL -H "Authorization: token ${token}" -o "${tmp}/existing" "${url}" 2>/dev/null; then
    if [ -z "${uuid}" ] || ! curl -sSfL -H "Authorization: token ${token}" -o "${tmp}/existing" \
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
