#!/usr/bin/env bash
# Create (if needed) the Forgejo release for the v* tag at HEAD and upload the
# given files as assets. Used by .forgejo/workflows/release.yml.
#
# Env: FORGE_BASE, REPO_OWNER, REPO_NAME, and RELEASE_TOKEN or FORGEJO_TOKEN.
set -euo pipefail

token="${RELEASE_TOKEN:-${FORGEJO_TOKEN:-}}"
if [ -z "${token}" ]; then
  echo "ERROR: neither RELEASE_TOKEN nor FORGEJO_TOKEN set; cannot publish release" >&2
  exit 1
fi

git fetch --force --tags origin >/dev/null 2>&1 || true
tag=$(git tag --points-at HEAD | grep '^v' | head -n1 || true)
if [ -z "${tag}" ]; then
  echo "ERROR: no v* tag points at HEAD; refusing to publish" >&2
  exit 1
fi

api="${FORGE_BASE}/api/v1/repos/${REPO_OWNER}/${REPO_NAME}"
auth="Authorization: token ${token}"

rid=$(curl -sS -H "${auth}" "${api}/releases/tags/${tag}" | jq -r '.id // empty' || true)
if [ -z "${rid}" ]; then
  echo "creating release ${tag}"
  rid=$(curl -sS -H "${auth}" -H 'Content-Type: application/json' \
    -X POST "${api}/releases" \
    -d "$(jq -n --arg t "${tag}" '{tag_name:$t, name:$t, draft:false, prerelease:false}')" \
    | jq -r '.id')
fi
if [ -z "${rid}" ] || [ "${rid}" = "null" ]; then
  echo "ERROR: could not create or find release for ${tag}" >&2
  exit 1
fi

for f in "$@"; do
  name=$(basename "$f")
  echo "uploading ${name} to release ${tag} (id ${rid})"
  # Replace an existing asset of the same name so re-runs are idempotent.
  aid=$(curl -sS -H "${auth}" "${api}/releases/${rid}/assets" | jq -r --arg n "${name}" '.[] | select(.name==$n) | .id' | head -n1 || true)
  if [ -n "${aid}" ]; then
    curl -sS -H "${auth}" -X DELETE "${api}/releases/${rid}/assets/${aid}" >/dev/null
  fi
  curl -sS -H "${auth}" -X POST "${api}/releases/${rid}/assets?name=${name}" -F "attachment=@${f}" >/dev/null
done
echo "done"
