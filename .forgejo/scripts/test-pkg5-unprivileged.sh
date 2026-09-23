#!/usr/bin/env bash
# Unprivileged pkg5 compatibility preflight: the native pkg5 client (pkg(1),
# pkgrecv) against this checkout's pkg6repo and pkg6depotd. Meant for a
# shared build host such as the oibuild zone, as an ordinary user:
#   * the release .p5p and a locally published dependency-free dummy are
#     imported with pkg6repo import-pkg5 into a scratch repository; importing
#     the release archive again reports "archive already imported" and leaves
#     every path and checksum of the repository unchanged;
#   * pkg6depotd serves that repository on a loopback high port, started in
#     the foreground by this script (no SMF);
#   * a fresh user image (pkg image-create -U --no-refresh, then refresh
#     --full) lists exactly the source FMRIs, and pkg info -r fetches the
#     release manifest;
#   * pkgrecv lists the same FMRIs and pulls both packages into a scratch
#     .p5p whose manifests are byte-identical and payloads content-identical
#     to the sources (pkg5_smoke_compare);
#   * the dummy installs into the user image, matches its proto, passes
#     pkg verify and uninstalls again.
#
# It never uses sudo, SMF, or the live image: every pkg(1) call names a
# scratch image with -R, and repositories, images, config and logs all live
# in one mktemp directory. Cleanup stops only the depot it started and removes
# only that directory. It is the pre-tag fast gate for pkg5 compatibility
# (docs/release-process.md); it does not replace the root-only live-package
# test with SMF, test-ips-package.sh, which the release CI runs on a
# throwaway VM.
#
# Usage: test-pkg5-unprivileged.sh P5P
# Environment:
#   PKG6_BIN_DIR       pkg6repo/pkg6depotd location (default: target/release)
#   PKG6_SMOKE_PORT    loopback port for the depot (default: a random free
#                      high port, retried if another process takes it)
#   PKG6_SMOKE_TMPDIR  parent of the scratch directory (default: /var/tmp)
#   PKG6_SMOKE_KEEP=1  keep the scratch directory (the depot is still stopped)
set -euo pipefail

[ "$#" -eq 1 ] || { echo "usage: $0 P5P" >&2; exit 2; }
[ "$(id -u)" != 0 ] || { echo "run $0 as an ordinary user, not root" >&2; exit 2; }

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "${here}/../.." && pwd)
# shellcheck source=release-lib.sh
. "${here}/release-lib.sh"
# shellcheck source=pkg5-smoke-lib.sh
. "${here}/pkg5-smoke-lib.sh"

die() { echo "FAIL: $*" >&2; exit 1; }

[ -f "$1" ] || die "release archive '$1' does not exist"
p5p=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
bin=${PKG6_BIN_DIR:-${root}/target/release}
pkg6repo=${bin}/pkg6repo
pkg6depotd=${bin}/pkg6depotd
for b in "${pkg6repo}" "${pkg6depotd}"; do
  [ -x "${b}" ] || die "${b} is missing; build it (cargo build --release) or set PKG6_BIN_DIR"
done
if [ -n "${PKG6_SMOKE_PORT:-}" ]; then
  case "${PKG6_SMOKE_PORT}" in
    *[!0-9]*) die "PKG6_SMOKE_PORT='${PKG6_SMOKE_PORT}' is not a port number" ;;
  esac
  [ "${PKG6_SMOKE_PORT}" -ge 1024 ] && [ "${PKG6_SMOKE_PORT}" -le 65535 ] \
    || die "PKG6_SMOKE_PORT=${PKG6_SMOKE_PORT} is not an unprivileged port (1024-65535)"
fi
for t in pkg pkgrecv pkgsend pkgrepo curl gzip cmp tar find cksum awk diff readlink mktemp; do
  command -v "${t}" >/dev/null 2>&1 || die "required tool '${t}' is not in PATH"
done

smoke=$(mktemp -d "${PKG6_SMOKE_TMPDIR:-/var/tmp}/pkg6-pkg5-unpriv.XXXXXX")
case "$(basename "${smoke}")" in
  pkg6-pkg5-unpriv.?*) [ -d "${smoke}" ] && [ -O "${smoke}" ] ;;
  *) false ;;
esac || die "unexpected scratch directory '${smoke}'"
depot_pid=
stop_depot() {
  local n=0
  [ -n "${depot_pid}" ] || return 0
  if kill -0 "${depot_pid}" 2>/dev/null; then
    kill "${depot_pid}" 2>/dev/null || true
    while [ "${n}" -lt 10 ] && kill -0 "${depot_pid}" 2>/dev/null; do
      n=$((n + 1))
      sleep 1
    done
    kill -9 "${depot_pid}" 2>/dev/null || true
  fi
  wait "${depot_pid}" 2>/dev/null || true
  depot_pid=
}
cleanup() {
  local rc=$?
  # A second Ctrl-C must not interrupt stopping the depot or removing scratch.
  trap '' INT TERM
  stop_depot
  if [ "${rc}" -ne 0 ] && [ -f "${smoke}/depot.log" ]; then
    echo "=== last lines of ${smoke}/depot.log" >&2
    tail -n 30 "${smoke}/depot.log" >&2 || true
  fi
  if [ "${PKG6_SMOKE_KEEP:-}" = 1 ]; then
    echo "kept scratch directory ${smoke}" >&2
  else
    rm -rf -- "${smoke}"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

mkdir "${smoke}/tmp" "${smoke}/src"
# p5 CMD...: run a pkg5 tool with scratch temp space, no proxy between it and
# the loopback depot, and no default image to fall back on.
p5() { (unset PKG_IMAGE; TMPDIR="${smoke}/tmp" no_proxy=127.0.0.1 NO_PROXY=127.0.0.1 "$@"); }

echo "=== sources"
cp "${p5p}" "${smoke}/src/release.p5p"
rel_fmri=$(p5 pkgrecv -s "${smoke}/src/release.p5p" --newest | grep -F "/${IPS_PKG_NAME}@" || true)
[ "$(echo "${rel_fmri}" | grep -c .)" = 1 ] || die "${p5p} does not hold exactly one ${IPS_PKG_NAME}: '${rel_fmri}'"
pub=${rel_fmri#pkg://}
pub=${pub%%/*}
[ -n "${pub}" ] && [ "${pub}" != "${rel_fmri}" ] || die "cannot tell the publisher of ${rel_fmri}"

# The dummy is owned by the running user, so an unprivileged install into a
# user image can set (and pkg verify can confirm) its ownership.
dummy=pkg6-release-test/pkg5-unprivileged
owner=$(id -un)
group=$(id -gn)
mkdir -p "${smoke}/dummy-proto/opt/pkg6-pkg5-smoke"
echo "pkg6 unprivileged pkg5 smoke ${smoke}" >"${smoke}/dummy-proto/opt/pkg6-pkg5-smoke/hello.txt"
cat >"${smoke}/dummy.p5m" <<EOF
set name=pkg.fmri value=${dummy}@1.0,5.11-0
set name=pkg.summary value="pkg6 release test: unprivileged pkg5 smoke"
dir path=opt owner=${owner} group=${group} mode=0755
dir path=opt/pkg6-pkg5-smoke owner=${owner} group=${group} mode=0755
file opt/pkg6-pkg5-smoke/hello.txt path=opt/pkg6-pkg5-smoke/hello.txt owner=${owner} group=${group} mode=0444
EOF
p5 pkgrepo create "${smoke}/dummy-repo"
p5 pkgrepo -s "${smoke}/dummy-repo" set "publisher/prefix=${pub}"
p5 pkgsend -s "${smoke}/dummy-repo" publish -d "${smoke}/dummy-proto" "${smoke}/dummy.p5m" \
  || die "pkgsend could not publish the dummy package"
p5 pkgrecv -s "${smoke}/dummy-repo" -a -d "${smoke}/src/dummy.p5p" "${dummy}" \
  || die "pkgrecv could not archive the dummy package"
expected=$({ p5 pkgrecv -s "${smoke}/src/release.p5p" --newest && p5 pkgrecv -s "${smoke}/src/dummy.p5p" --newest; } | sort) \
  || die "pkgrecv could not list the source archives"
dummy_fmri=$(echo "${expected}" | grep -F "pkg://${pub}/${dummy}@" || true)
[ "$(echo "${dummy_fmri}" | grep -c .)" = 1 ] || die "dummy archive does not hold ${dummy} for ${pub}: ${expected}"
echo "source FMRIs:"
echo "${expected}"

echo "=== import both archives with ${pkg6repo}"
repo="${smoke}/repo"
"${pkg6repo}" create "${repo}"
for a in release dummy; do
  RUST_LOG=info "${pkg6repo}" import-pkg5 -s "${smoke}/src/${a}.p5p" -d "${repo}" \
    || die "import-pkg5 of the ${a} archive failed"
done
tree_before=$(pkg5_smoke_tree "${repo}") || die "cannot snapshot ${repo}"
grep -q '^f ' <<<"${tree_before}" || die "repository holds no files after the import"
reimport_log=$(RUST_LOG=info "${pkg6repo}" import-pkg5 -s "${smoke}/src/release.p5p" -d "${repo}" 2>&1) \
  || die "re-importing the same archive failed: ${reimport_log}"
echo "${reimport_log}"
grep -qF 'archive already imported' <<<"${reimport_log}" \
  || die "re-import did not report the archive as already imported"
tree_after=$(pkg5_smoke_tree "${repo}") || die "cannot snapshot ${repo}"
[ "${tree_after}" = "${tree_before}" ] \
  || die "re-import changed the repository: $(diff <(echo "${tree_before}") <(echo "${tree_after}") || true)"
echo "re-import left the repository unchanged ($(echo "${tree_before}" | grep -c .) entries)"

echo "=== start ${pkg6depotd}"
# start_depot PORT: start the depot on 127.0.0.1:PORT and wait until it has
# bound the port itself (its own log says so), not merely until something
# answers there. Returns 1 if the port is taken, dies on any other failure.
start_depot() {
  local p="$1"
  cat >"${smoke}/depot.kdl" <<EOF
server {
    bind "127.0.0.1:${p}"
}
repository {
    root "${repo}"
    mode "readonly"
}
telemetry {
    service-name "pkg6depotd-pkg5-smoke"
    log-format "text"
}
EOF
  "${pkg6depotd}" -c "${smoke}/depot.kdl" config-test >"${smoke}/depot.log" 2>&1 \
    || die "pkg6depotd rejects ${smoke}/depot.kdl: $(cat "${smoke}/depot.log")"
  RUST_LOG=info "${pkg6depotd}" -c "${smoke}/depot.kdl" >>"${smoke}/depot.log" 2>&1 &
  depot_pid=$!
  local n=0
  while [ "${n}" -lt 30 ]; do
    n=$((n + 1))
    if grep -qF "Starting pkg6depotd on 127.0.0.1:${p}" "${smoke}/depot.log"; then
      return 0
    fi
    kill -0 "${depot_pid}" 2>/dev/null || break
    sleep 1
  done
  if ! kill -0 "${depot_pid}" 2>/dev/null && grep -qi 'address already in use' "${smoke}/depot.log"; then
    stop_depot
    return 1
  fi
  die "pkg6depotd did not start on 127.0.0.1:${p}"
}
if [ -n "${PKG6_SMOKE_PORT:-}" ]; then
  port=${PKG6_SMOKE_PORT}
  start_depot "${port}" || die "port ${port} is already in use"
else
  port=
  n=0
  while [ "${n}" -lt 10 ]; do
    n=$((n + 1))
    p=$((20000 + (RANDOM * 32768 + RANDOM) % 40000))
    if start_depot "${p}"; then port=${p}; break; fi
    echo "port ${p} is taken, trying another"
  done
  [ -n "${port}" ] || die "found no free loopback port in 10 attempts; set PKG6_SMOKE_PORT"
fi
url="http://127.0.0.1:${port}"
curl --noproxy 127.0.0.1 -sSf "${url}/versions/0/" >/dev/null || die "depot does not answer ${url}/versions/0/"
curl --noproxy 127.0.0.1 -sSf "${url}/${pub}/catalog/1/catalog.attrs" >/dev/null || die "depot does not serve the ${pub} catalog"
echo "depot pid ${depot_pid} serves ${repo} on ${url}"

echo "=== pkg(1): catalog and manifest in a scratch user image"
img="${smoke}/image"
p5 pkg image-create -U --no-refresh -p "${pub}=${url}/" "${img}" || die "pkg image-create failed"
p5 pkg -R "${img}" refresh --full || die "pkg(1) could not read the catalog from ${url}"
got=$(p5 pkg -R "${img}" list -Hafv | awk '{ print $1 }' | sort) || die "pkg list -af found no packages in ${url}"
[ "${got}" = "${expected}" ] || die "pkg(1) catalog lists '${got}', the sources hold '${expected}'"
p5 pkg -R "${img}" info -r "${rel_fmri}" || die "pkg info -r could not fetch the manifest of ${rel_fmri}"

echo "=== pkgrecv: manifests and payloads into a scratch archive"
got=$(p5 pkgrecv -s "${url}/" --newest | sort) || die "pkgrecv could not list ${url}"
[ "${got}" = "${expected}" ] || die "pkgrecv lists '${got}', the sources hold '${expected}'"
p5 pkgrecv -s "${url}/" -c "${smoke}/pkgrecv-cache" -a -d "${smoke}/received.p5p" "${rel_fmri}" "${dummy_fmri}" \
  || die "pkgrecv could not retrieve the packages from ${url}"
for a in src/release src/dummy received; do
  pkg5_smoke_unpack "${smoke}/${a}.p5p" "${smoke}/unpacked-${a#src/}" || die "cannot unpack ${smoke}/${a}.p5p"
done
diffs=$(pkg5_smoke_compare "${smoke}/unpacked-received" "${smoke}/unpacked-release" "${smoke}/unpacked-dummy") \
  || die "pkgrecv copy from the depot differs from the sources: ${diffs}"
echo "manifests byte-identical, payloads content-identical:"
pkg5_smoke_list pkg "${smoke}/unpacked-received"

echo "=== pkg(1): install, verify and uninstall the dummy in the user image"
p5 pkg -R "${img}" install -v "${dummy_fmri}" || die "pkg install of ${dummy_fmri} from ${url} failed"
[ "$(p5 pkg -R "${img}" list -Hv "${dummy}" | awk '{ print $1 }')" = "${dummy_fmri}" ] \
  || die "scratch image does not have ${dummy_fmri} installed"
cmp "${img}/opt/pkg6-pkg5-smoke/hello.txt" "${smoke}/dummy-proto/opt/pkg6-pkg5-smoke/hello.txt" \
  || die "installed dummy file differs from its proto"
p5 pkg -R "${img}" verify -v "${dummy}" || die "pkg verify of ${dummy} failed"
p5 pkg -R "${img}" uninstall -v "${dummy}" || die "pkg uninstall of ${dummy} failed"
[ ! -e "${img}/opt/pkg6-pkg5-smoke/hello.txt" ] || die "dummy file still present after uninstall"

kill -0 "${depot_pid}" 2>/dev/null || die "pkg6depotd exited during the smoke"
echo "PASS: unprivileged pkg5 client smoke against pkg6depotd on ${url}"
