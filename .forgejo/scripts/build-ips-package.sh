#!/usr/bin/env bash
# Build the package/pkg6/depotd IPS package on OpenIndiana from the release
# binaries in target/release. Uses only pkg5's own publication tools, which
# ship in package/pkg on every OpenIndiana system.
#
# Usage: build-ips-package.sh OUTDIR
#
# Produces in OUTDIR:
#   pkg6depotd-<version>-illumos-x86_64.p5p   installable archive
#   pkg6depotd-<version>-illumos-x86_64.p5m   resolved, published manifest
# and leaves a scratch repository in target/ips/repo plus the proto area in
# target/ips/proto for test-ips-package.sh.
#
# Nothing is installed or changed outside the checkout.
#
# Local dry runs on an untagged checkout: RELEASE_TAG_OVERRIDE=v1.2.3-rc.1
# RELEASE_ALLOW_UNTAGGED=1 (the FMRI then records HEAD).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=release-lib.sh
. "${here}/release-lib.sh"

[ "$#" -eq 1 ] || { echo "usage: $0 OUTDIR" >&2; exit 2; }
outdir="$1"
mkdir -p "${outdir}"
outdir=$(cd "${outdir}" && pwd)

for tool in pkgmogrify pkgdepend pkglint pkgrepo pkgsend pkgrecv; do
  command -v "${tool}" >/dev/null || release_die "${tool} not found; build on OpenIndiana with package/pkg installed"
done

release_init
root=$(git rev-parse --show-toplevel)
head=$(git rev-parse --verify HEAD)
if tag_commit=$(git rev-parse --verify --quiet "refs/tags/${RELEASE_TAG}^{commit}"); then
  [ "${tag_commit}" = "${head}" ] || release_die "tag ${RELEASE_TAG} is ${tag_commit}, HEAD is ${head}"
elif [ "${RELEASE_ALLOW_UNTAGGED:-}" != 1 ]; then
  release_die "tag ${RELEASE_TAG} not present locally; fetch it or set RELEASE_ALLOW_UNTAGGED=1 for a dry run"
fi
RELEASE_COMMIT="${head}"
IPS_FMRI=$(release_ips_fmri "$(release_ips_timestamp "${RELEASE_COMMIT}")")
echo "building ${IPS_FMRI}"

bindir="${CARGO_TARGET_DIR:-${root}/target}/release"
work="${root}/target/ips"
proto="${work}/proto"
repo="${work}/repo"
rm -rf "${work}"
mkdir -p "${proto}"

# stage SRC DEST: copy into the proto area. Modes and ownership come from the
# manifest, not from the proto files.
stage() {
  mkdir -p "$(dirname "${proto}/$2")"
  cp "$1" "${proto}/$2"
}
stage "${bindir}/pkg6depotd" usr/lib/pkg6depotd
for b in pkg6repo pkg6recv pkgtree; do
  stage "${bindir}/${b}" "usr/bin/${b}"
done
stage "${root}/packaging/ips/pkg6depotd.kdl" etc/pkg6depotd/pkg6depotd.kdl
stage "${root}/packaging/ips/pkg6depotd.xml" lib/svc/manifest/application/pkg/pkg6depotd.xml
stage "${root}/packaging/ips/svc-pkg6depotd" lib/svc/method/svc-pkg6depotd
stage "${root}/packaging/ips/pkg6depotd.8" usr/share/man/man8/pkg6depotd.8
stage "${root}/pkg6repo/pkg6repo.1" usr/share/man/man1/pkg6repo.1
stage "${root}/man/man5/pkg6-policy.5" usr/share/man/man5/pkg6-policy.5
stage "${root}/man/man8/pkg6repo-retire.8" usr/share/man/man8/pkg6repo-retire.8

# 1. Expand the template.
pkgmogrify \
  -D "IPS_FMRI=${IPS_FMRI}" \
  -D "RELEASE_VERSION=${RELEASE_VERSION}" \
  -D "RELEASE_TAG=${RELEASE_TAG}" \
  -D "RELEASE_COMMIT=${RELEASE_COMMIT}" \
  "${root}/packaging/ips/pkg6depotd.p5m" > "${work}/pkg6depotd.mog"
unexpanded=$(release_unexpanded_macros "${work}/pkg6depotd.mog")
[ -z "${unexpanded}" ] || release_die "unexpanded macros in the manifest:"$'\n'"${unexpanded}"

# 2. Generate and resolve dependencies (ELF, SMF, #! interpreters) against
#    the build host's installed packages.
pkgdepend generate -md "${proto}" -d "${root}" "${work}/pkg6depotd.mog" > "${work}/pkg6depotd.dep"
pkgdepend resolve -m "${work}/pkg6depotd.dep"
resolved="${work}/pkg6depotd.dep.res"
# pkgdepend derives org.opensolaris.smf.fmri from the SMF manifest; the
# template must not add its own (pkglint.manifest006 on duplicates).
n=$(release_smf_fmri_actions "${resolved}")
[ "${n}" = 1 ] || release_die "expected one generated org.opensolaris.smf.fmri action, found ${n}"
grep '^set name=org\.opensolaris\.smf\.fmri ' "${resolved}" | grep -q 'svc:/application/pkg/pkg6depotd:default' \
  || release_die "generated org.opensolaris.smf.fmri does not name svc:/application/pkg/pkg6depotd:default"
echo "--- resolved dependencies"
grep '^depend ' "${resolved}" || release_die "pkgdepend resolved no dependencies; the ELF analysis failed"

# 3. Lint: show everything, fail on errors.
echo "--- pkglint (informational)"
pkglint "${resolved}" || true
echo "--- pkglint (gate: errors only)"
pkglint -f "${root}/packaging/ips/pkglintrc" "${resolved}"

# pkg5_quote S: S %-quoted as pkg5 names manifest paths (urllib quote with
# no safe characters, fmri.get_dir_path()).
pkg5_quote() {
  printf '%s' "$1" | grep -q '^[A-Za-z0-9_.~,:/%-]*$' || release_die "cannot quote '$1'"
  printf '%s' "$1" | sed -e 's/%/%25/g' -e 's|/|%2F|g' -e 's/,/%2C/g' -e 's/:/%3A/g'
}
tar=tar
command -v gtar >/dev/null 2>&1 && tar=gtar
ips_stem_version="${IPS_FMRI#pkg://"${IPS_PUBLISHER}"/}"
[ "${ips_stem_version}" != "${IPS_FMRI}" ] || release_die "${IPS_FMRI} does not name publisher ${IPS_PUBLISHER}"
ips_quoted_stem=$(pkg5_quote "${ips_stem_version%%@*}")
ips_quoted_version=$(pkg5_quote "${ips_stem_version#*@}")
ips_manifest_path="publisher/${IPS_PUBLISHER}/pkg/${ips_quoted_stem}/${ips_quoted_version}"

# check_fmri REPO_OR_P5P WHAT: die unless the source holds exactly one
# package manifest, stored under IPS_FMRI's full version (build release and
# timestamp included) and declaring pkg.fmri=IPS_FMRI. A repository and a
# p5p share the layout publisher/PUB/pkg/STEM/VERSION. pkgrecv --newest is
# only a cross-check: it prints the FMRI without the build release.
check_fmri() {
  local src="$1" what="$2" got check
  got=$(pkgrecv -s "${src}" --newest) || release_die "pkgrecv --newest failed on the ${what}"
  [ "${got}" = "${IPS_FMRI/,"${IPS_BUILD_RELEASE}"-/-}" ] \
    || release_die "pkgrecv --newest on the ${what} lists"$'\n'"${got}"$'\n'"instead of exactly"$'\n'"${IPS_FMRI} (less the build release)"
  if [ -d "${src}" ]; then
    got=$(cd "${src}" && find publisher -type f) || release_die "cannot list the ${what}"
  else
    got=$("${tar}" -tf "${src}") || release_die "cannot list the ${what}"
  fi
  got=$(printf '%s\n' "${got}" | grep -E '^publisher/[^/]+/pkg/[^/]+/[^/]+$' || true)
  [ "${got}" = "${ips_manifest_path}" ] \
    || release_die "the ${what} holds the manifests"$'\n'"${got}"$'\n'"instead of exactly"$'\n'"${ips_manifest_path}"
  check="${work}/check-fmri"
  rm -rf "${check}"
  mkdir "${check}"
  if [ -d "${src}" ]; then
    cp "${src}/${ips_manifest_path}" "${check}/manifest"
  else
    (cd "${check}" && "${tar}" -xf "${src}" "${ips_manifest_path}") \
      && mv "${check}/${ips_manifest_path}" "${check}/manifest"
  fi || release_die "cannot read ${ips_manifest_path} from the ${what}"
  got=$(grep '^set name=pkg\.fmri ' "${check}/manifest" || true)
  [ "${got}" = "set name=pkg.fmri value=${IPS_FMRI}" ] \
    || release_die "the ${what} manifest declares"$'\n'"${got}"$'\n'"instead of"$'\n'"set name=pkg.fmri value=${IPS_FMRI}"
  rm -rf "${check}"
}

# 4. Publish into a scratch repository and verify it. pkgsend replaces the
#    manifest's pkg.fmri timestamp with the publication time unless the
#    global -D allow-timestamp comes before the subcommand (pkg5
#    publish.py); with it, the timestamp is the tagged commit's, so a
#    rebuild of the same tag yields the same FMRI.
pkgrepo create "${repo}"
pkgrepo -s "${repo}" set "publisher/prefix=${IPS_PUBLISHER}"
pkgsend -D allow-timestamp -s "${repo}" publish -d "${proto}" -d "${root}" "${resolved}"
check_fmri "${repo}" "scratch repository"
# Integrity only: the scratch repository holds just this package, so its
# dependencies on OS packages (SUNWcs, shells, system libraries) cannot
# resolve here. test-ips-package.sh checks dependencies by installing the
# package into a real image.
pkgrepo verify --disable dependency -s "${repo}"
pkgrepo -s "${repo}" list

# 5. Archive, and check the FMRI the archive actually carries before
#    anything (fmri.env, provenance, dist) records IPS_FMRI.
archive="${work}/$(release_p5p_name)"
pkgrecv -s "${repo}" -a -d "${archive}" "${IPS_PKG_NAME}"
pkgrepo -s "${archive}" list
check_fmri "${archive}" "p5p archive"
p5p="${outdir}/$(release_p5p_name)"
rm -f "${p5p}"
mv "${archive}" "${p5p}"
cp "${resolved}" "${outdir}/$(release_manifest_name)"

echo "IPS_FMRI=${IPS_FMRI}" > "${work}/fmri.env"
echo "built ${p5p}"
