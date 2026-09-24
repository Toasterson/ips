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
# IPS_BASELINE_IMAGE (required) is the root of a pkg(5) image at the oldest
# supported OS level: every IPS_BASELINE_PINS package (release-lib.sh), both
# incorporations included, must be installed there at exactly its pinned
# version. Dependencies are resolved against the packages installed there,
# not against the build host, and every resolved minimum version must equal
# the version the baseline has; then a pkg install -n into the baseline
# proves the archive installs there. The baseline is only read: the dry run
# changes nothing in it. See packaging/ips/README.md, Dependency baseline,
# for how to create one.
#
# Local dry runs on an untagged checkout: RELEASE_TAG_OVERRIDE=v1.2.3-rc.1
# RELEASE_ALLOW_UNTAGGED=1 (the manifest's info.repository-changeset then
# records HEAD).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=release-lib.sh
. "${here}/release-lib.sh"

[ "$#" -eq 1 ] || { echo "usage: $0 OUTDIR" >&2; exit 2; }
outdir="$1"
mkdir -p "${outdir}"
outdir=$(cd "${outdir}" && pwd)

for tool in pkg pkgmogrify pkgdepend pkglint pkgrepo pkgsend pkgrecv; do
  command -v "${tool}" >/dev/null || release_die "${tool} not found; build on OpenIndiana with package/pkg installed"
done

# The dependency baseline must exist and be at exactly the supported OS
# level; a newer image (the build host's own, say) would raise every OS
# minimum version past what the oldest supported target can install.
[ -n "${IPS_BASELINE_IMAGE:-}" ] \
  || release_die "IPS_BASELINE_IMAGE is not set; point it at a pkg(5) image with the IPS_BASELINE_PINS of release-lib.sh (packaging/ips/README.md, Dependency baseline)"
[ -d "${IPS_BASELINE_IMAGE}" ] || release_die "IPS_BASELINE_IMAGE=${IPS_BASELINE_IMAGE} is not a directory"
baseline=$(cd "${IPS_BASELINE_IMAGE}" && pwd)
# baseline_version STEM: the version of STEM installed in the baseline, in
# the form pkgdepend writes minimum versions.
baseline_version() {
  local fmri
  fmri=$(pkg -R "${baseline}" list -Hv "$1" | awk '{ print $1 }') \
    || { release_die "$1 is not installed in the baseline image ${baseline}, or it is not a pkg(5) image"; return 1; }
  release_fmri_version "${fmri}" \
    || { release_die "cannot read the $1 version of the baseline image ${baseline}"; return 1; }
}
while read -r pin; do
  have=$(baseline_version "${pin%@*}")
  [ "${have}" = "${pin#*@}" ] \
    || release_die "baseline image ${baseline} has ${pin%@*}@${have}, the supported baseline is ${pin}"
  echo "dependency baseline: ${baseline} has ${pin}"
done <<<"${IPS_BASELINE_PINS}"

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
# pkgtree is not packaged: developer/pkgtree owns usr/bin/pkgtree on OpenIndiana.
for b in pkg6repo pkg6recv; do
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

# 2. Generate dependencies (ELF, SMF, #! interpreters) and resolve them
#    against the packages installed in the baseline image, never the build
#    host's: pkgdepend writes each dependency's installed version as the
#    minimum. A file the baseline delivers from no package is an error.
pkgdepend generate -md "${proto}" -d "${root}" "${work}/pkg6depotd.mog" > "${work}/pkg6depotd.dep"
pkgdepend -R "${baseline}" resolve -m "${work}/pkg6depotd.dep" \
  || release_die "pkgdepend could not resolve every dependency against the baseline image ${baseline}"
resolved="${work}/pkg6depotd.dep.res"
# Every versioned dependency must require exactly what the baseline has.
for dep in $(awk '$1 == "depend" { for (i = 2; i <= NF; i++) if ($i ~ /^fmri=/) print substr($i, 6) }' "${resolved}"); do
  case "${dep}" in *@*) ;; *) continue ;; esac
  stem="${dep#pkg:/}"
  stem="${stem#/*/}"
  stem="${stem%%@*}"
  min=$(release_fmri_version "${dep}")
  have=$(baseline_version "${stem}")
  [ "${min}" = "${have}" ] \
    || release_die "the package would require ${stem}@${min}, but the baseline image ${baseline} has ${stem}@${have}"
done
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

# 6. The archive must install into the baseline: a dry run (-n changes
#    nothing) against its installed incorporations, with the catalogs it
#    already has.
pkg -R "${baseline}" install -nv --no-refresh -g "${archive}" "${IPS_PKG_NAME}" \
  || release_die "${IPS_FMRI} does not install into the baseline image ${baseline}"
p5p="${outdir}/$(release_p5p_name)"
rm -f "${p5p}"
mv "${archive}" "${p5p}"
cp "${resolved}" "${outdir}/$(release_manifest_name)"

echo "IPS_FMRI=${IPS_FMRI}" > "${work}/fmri.env"
echo "built ${p5p}"
