#!/usr/bin/env bash
# Pack the release binaries (RELEASE_BINARIES) into a reproducible tarball:
# sorted entries, numeric root ownership, mtimes pinned to the tagged commit,
# gzip without a timestamp. Rebuilding the same commit with the same
# toolchain gives the same bytes, so a re-run can match assets that were
# already uploaded.
#
# Usage: package-tarball.sh PLATFORM OUTDIR
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=release-lib.sh
. "${here}/release-lib.sh"

[ "$#" -eq 2 ] || { echo "usage: $0 PLATFORM OUTDIR" >&2; exit 2; }
platform="$1"
mkdir -p "$2"
outdir=$(cd "$2" && pwd)

release_init
root=$(git rev-parse --show-toplevel)
bindir="${CARGO_TARGET_DIR:-${root}/target}/release"
sde=$(release_source_date_epoch)

if command -v gtar >/dev/null 2>&1; then tar=gtar; else tar=tar; fi
"${tar}" --version 2>/dev/null | grep -q 'GNU tar' || release_die "GNU tar is required"

name="ips-${RELEASE_VERSION}-${platform}"
staging=$(mktemp -d)
trap 'rm -rf "${staging}"' EXIT
mkdir -p "${staging}/${name}/bin"
for b in ${RELEASE_BINARIES}; do
  cp "${bindir}/${b}" "${staging}/${name}/bin/${b}"
  chmod 0755 "${staging}/${name}/bin/${b}"
done
cp "${root}/LICENSE" "${root}/README.md" "${staging}/${name}/"
chmod 0644 "${staging}/${name}/LICENSE" "${staging}/${name}/README.md"

out="${outdir}/$(release_tarball_name "${platform}")"
"${tar}" -C "${staging}" --sort=name --mtime="@${sde}" --owner=0 --group=0 \
  --numeric-owner --format=gnu -cf - "${name}" | gzip -n -9 > "${out}"
echo "built ${out}"
