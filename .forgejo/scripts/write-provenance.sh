#!/usr/bin/env bash
# Write SHA256SUMS-<platform> and provenance-<platform>.json for the given
# release files into OUTDIR. Must run after release_verify_commit succeeded in
# the same checkout (it re-verifies HEAD against the tag locally).
#
# Both files are a pure function of the tagged source, the toolchain and the
# artifact bytes. Per-run facts (host name, runner request, kernel build) go
# to the job log only, so a clean re-run of the same tag writes identical
# files and the immutable-asset upload can match them.
#
# Usage: write-provenance.sh PLATFORM OUTDIR FILE...
# Optional env: IPS_FMRI (recorded when the platform built an IPS package).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=release-lib.sh
. "${here}/release-lib.sh"

[ "$#" -ge 3 ] || { echo "usage: $0 PLATFORM OUTDIR FILE..." >&2; exit 2; }
platform="$1"
outdir="$2"
shift 2

release_init
release_platform_assets "${platform}" >/dev/null

head=$(git rev-parse --verify HEAD)
tag_commit=$(git rev-parse --verify "refs/tags/${RELEASE_TAG}^{commit}")
[ "${head}" = "${tag_commit}" ] || release_die "HEAD ${head} is not tag ${RELEASE_TAG} (${tag_commit})"
tag_object=$(git rev-parse --verify "refs/tags/${RELEASE_TAG}")
tag_type=$(git cat-file -t "${tag_object}")
tree=$(git rev-parse --verify "HEAD^{tree}")
sde=$(git show -s --format=%ct HEAD)

sums="${outdir}/$(release_sums_name "${platform}")"
prov="${outdir}/$(release_provenance_name "${platform}")"
: > "${sums}"
artifacts='[]'
for f in "$@"; do
  name=$(basename "${f}")
  sum=$(release_sha256 "${f}")
  size=$(wc -c < "${f}" | tr -d ' ')
  printf '%s  %s\n' "${sum}" "${name}" >> "${sums}"
  artifacts=$(printf '%s' "${artifacts}" | jq -c --arg n "${name}" --arg s "${sum}" --argjson z "${size}" \
    '. + [{name: $n, sha256: $s, size: $z}]')
done

# Per-run context, for the log only.
echo "runner: request=${SOLSTICE_REQUEST_ID:-} ref=${GITHUB_REF:-} sha=${GITHUB_SHA:-} host=$(uname -n)"

jq -n \
  --arg tag "${RELEASE_TAG}" \
  --arg tag_object "${tag_object}" \
  --arg tag_type "${tag_type}" \
  --arg commit "${head}" \
  --arg tree "${tree}" \
  --arg version "${RELEASE_VERSION}" \
  --arg channel "${RELEASE_CHANNEL}" \
  --argjson prerelease "${RELEASE_PRERELEASE}" \
  --arg platform "${platform}" \
  --argjson source_date_epoch "${sde}" \
  --arg repo "$(release_public_repo_url)" \
  --arg os "$(uname -s) $(uname -m)" \
  --arg rustc "$(rustc -Vv)" \
  --arg cargo "$(cargo -V)" \
  --arg cargo_lock_sha256 "$(release_sha256 Cargo.lock)" \
  --arg ips_fmri "${IPS_FMRI:-}" \
  --argjson artifacts "${artifacts}" \
  '{
     schema: "ips-release-provenance/2",
     source: {repository: $repo, tag: $tag, tag_object: $tag_object,
              tag_type: $tag_type, commit: $commit, tree: $tree,
              source_date_epoch: $source_date_epoch,
              cargo_lock_sha256: $cargo_lock_sha256},
     release: {version: $version, channel: $channel, prerelease: $prerelease},
     build: {platform: $platform, os: $os, rustc: $rustc, cargo: $cargo},
     ips_fmri: (if $ips_fmri == "" then null else $ips_fmri end),
     artifacts: $artifacts
   }' > "${prov}"

echo "wrote ${sums} and ${prov}"
cat "${sums}"
