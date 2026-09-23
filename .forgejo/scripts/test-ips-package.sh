#!/usr/bin/env bash
# Install, run, upgrade and remove package/pkg6/depotd on the LIVE image of
# the machine it runs on, and assert OpenIndiana service behaviour:
#   * the pkg6srv account (UID/GID 94) and file ownership/modes
#   * the four manual pages are installed as staged and render with man(1)
#   * the SMF manifest validates and imports with the instance disabled
#   * the service starts as pkg6srv and answers /versions/0/
#   * the native pkg5 client (pkg(1), pkgrecv) reads the catalog, manifests
#     and payloads of packages imported with pkg6repo import-pkg5 from the
#     running depot, and installs one into a scratch image (see "pkg5 client
#     smoke" below)
#   * an operator-edited config survives an upgrade (preserve=renamenew:
#     the new default lands in pkg6depotd.kdl.new) and the depot stays up
#   * uninstall disables the service and removes the payload, but leaves the
#     operator-created repository untouched; nothing under /var/pkg6depotd is
#     salvaged to lost+found (the edited config may be, as pkg(1) intends)
#
# This mutates the running system (packages, users, SMF). It is meant for the
# throwaway Solstice CI VM only and refuses to run unless
# PKG6_TEST_LIVE_IMAGE=yes-this-is-a-throwaway-vm is set. Never run it on a
# shared build host or a production zone.
#
# Usage: test-ips-package.sh P5P
# Needs target/ips/{proto,repo,pkg6depotd.dep.res} from build-ips-package.sh.
set -euo pipefail

[ "${PKG6_TEST_LIVE_IMAGE:-}" = yes-this-is-a-throwaway-vm ] || {
  echo "refusing to modify the live image; see the header of $0" >&2
  exit 2
}
[ "$#" -eq 1 ] || { echo "usage: $0 P5P" >&2; exit 2; }

here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=release-lib.sh
. "${here}/release-lib.sh"
release_init

p5p=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
root=$(git rev-parse --show-toplevel)
work="${root}/target/ips"
# The config property group is defined on the service, not on the default
# instance (pkg6depotd.xml), so svccfg setprop must target the service;
# `svccfg -s <instance> setprop` only sees instance-level groups and fails
# with "No such property group". Refresh/restart/clear act on the instance,
# which then sees the service value through its composed view.
svc_service=svc:/application/pkg/pkg6depotd
svc=${svc_service}:default
conf=/etc/pkg6depotd/pkg6depotd.kdl
port=18080

fail() { echo "FAIL: $*" >&2; dump; exit 1; }
# smf CMD ARGS...: run an SMF admin command as root; on failure report the
# command and the service diagnostics instead of a bare set -e exit.
smf() {
  local out
  out=$(sudo "$@" 2>&1) || fail "'$*' failed: ${out}"
  [ -z "${out}" ] || echo "${out}"
}
# set_config_file PATH: point config/file at PATH (service level), refresh
# the instance and confirm it now sees the value.
set_config_file() {
  local got
  smf svccfg -s "${svc_service}" setprop config/file = astring: "$1"
  smf svcadm refresh "${svc}"
  got=$(svcprop -p config/file "${svc}" 2>&1) || fail "svcprop config/file failed: ${got}"
  [ "${got}" = "$1" ] || fail "instance sees config/file='${got}' after refresh, expected '$1'"
}
dump() {
  echo "=== diagnostics"
  svcs -xv "${svc}" 2>&1 || true
  sudo tail -n 50 "/var/svc/log/application-pkg-pkg6depotd:default.log" 2>&1 || true
}
# wait_state STATE: poll the instance for up to 90 s.
wait_state() {
  local s
  for _ in $(seq 1 90); do
    s=$(svcs -H -o state "${svc}" 2>/dev/null || true)
    [ "${s}" = "$1" ] && return 0
    sleep 1
  done
  fail "${svc} is '${s}', expected '$1'"
}
# attr PATH: print "mode owner group" of PATH.
attr() {
  # shellcheck disable=SC2012
  ls -ld "$1" | awk '{ print $1, $3, $4 }'
}
expect_attr() {
  local got
  got=$(attr "$1")
  [ "${got}" = "$2" ] || fail "$1 has '${got}', expected '$2'"
}
probe() {
  local _
  for _ in $(seq 1 30); do
    if curl -sSf "http://127.0.0.1:${port}/versions/0/" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  fail "depot did not answer http://127.0.0.1:${port}/versions/0/"
}

echo "=== install from ${p5p}"
sudo pkg install -v -g "${p5p}" "${IPS_PKG_NAME}"
pkg list -v "${IPS_PKG_NAME}"
sudo pkg verify -v "${IPS_PKG_NAME}" || fail "pkg verify after install"

echo "=== account and files"
[ "$(getent passwd pkg6srv | cut -d: -f3,4)" = "94:94" ] || fail "pkg6srv is not uid/gid 94: $(getent passwd pkg6srv)"
getent group pkg6srv | grep -q '^pkg6srv:[^:]*:94:' || fail "group pkg6srv is not gid 94"
expect_attr /etc/pkg6depotd "drwxr-x--- root pkg6srv"
expect_attr "${conf}" "-rw-r----- root pkg6srv"
[ ! -e /var/pkg6depotd ] || fail "/var/pkg6depotd must not be delivered by the package"
expect_attr /usr/lib/pkg6depotd "-r-xr-xr-x root bin"
expect_attr /lib/svc/method/svc-pkg6depotd "-r-xr-xr-x root bin"
for b in pkg6repo pkg6recv pkgtree; do
  expect_attr "/usr/bin/${b}" "-r-xr-xr-x root bin"
done
/usr/bin/pkg6repo --version

echo "=== manual pages"
# Every page the package delivers is installed read-only, byte-identical to
# the staged source, and renders through the system man(1) from its section.
man_pages="man1/pkg6repo.1 man5/pkg6-policy.5 man8/pkg6depotd.8 man8/pkg6repo-retire.8"
for m in ${man_pages}; do
  f=/usr/share/man/${m}
  expect_attr "${f}" "-r--r--r-- root bin"
  cmp "${f}" "${work}/proto/usr/share/man/${m}" || fail "${f} differs from the staged page"
  n=$(basename "${m}")
  out=$(man -M /usr/share/man -s "${n##*.}" "${n%.*}" 2>&1 | col -b) \
    || fail "man -s ${n##*.} ${n%.*} failed: ${out}"
  echo "${out}" | grep -q '^NAME' || fail "man -s ${n##*.} ${n%.*} rendered no NAME section: ${out}"
done

echo "=== SMF"
svccfg validate /lib/svc/manifest/application/pkg/pkg6depotd.xml || fail "svccfg validate rejected the installed manifest"
wait_state disabled
[ "$(svcprop -p startd/duration "${svc}")" = child ] || fail "startd/duration is not child"

echo "=== operator setup: repository and config edit"
sudo mkdir -m 0750 /var/pkg6depotd
sudo chown pkg6srv:pkg6srv /var/pkg6depotd
sudo su pkg6srv -c "/usr/bin/pkg6repo create /var/pkg6depotd/repo"
sudo su pkg6srv -c "/usr/bin/pkg6repo add-publisher -s /var/pkg6depotd/repo test"
# Edit in place (keeps mode and owner), as an operator would.
sudo sh -c "sed 's/127.0.0.1:8080/127.0.0.1:${port}/' ${conf} > ${conf}.tmp && cat ${conf}.tmp > ${conf} && rm ${conf}.tmp"
grep -q "127.0.0.1:${port}" "${conf}" 2>/dev/null || sudo grep -q "127.0.0.1:${port}" "${conf}" || fail "config edit did not apply"

echo "=== start"
smf svcadm enable "${svc}"
wait_state online
pgrep -u pkg6srv -f /usr/lib/pkg6depotd >/dev/null || fail "pkg6depotd is not running as pkg6srv"
probe
pid1=$(pgrep -u pkg6srv -f /usr/lib/pkg6depotd)

echo "=== missing config puts the instance in maintenance"
set_config_file /etc/pkg6depotd/absent.kdl
smf svcadm restart "${svc}"
wait_state maintenance

echo "=== invalid (readable) config also fails closed in the daemon"
# Unterminated server block: readable, but not valid KDL.
broken=$(mktemp)
printf 'server {\n    bind "127.0.0.1:%s"\n' "${port}" > "${broken}"
sudo cp "${broken}" /etc/pkg6depotd/broken.kdl
rm -f "${broken}"
sudo chown root:pkg6srv /etc/pkg6depotd/broken.kdl
sudo chmod 0640 /etc/pkg6depotd/broken.kdl
set_config_file /etc/pkg6depotd/broken.kdl
smf svcadm clear "${svc}"
wait_state maintenance
if curl -sSf "http://127.0.0.1:${port}/versions/0/" >/dev/null 2>&1 \
  || curl -sSf "http://127.0.0.1:8080/versions/0/" >/dev/null 2>&1; then
  fail "something answers although the configuration is invalid"
fi
sudo rm -f /etc/pkg6depotd/broken.kdl

set_config_file "${conf}"
smf svcadm clear "${svc}"
wait_state online
probe

echo "=== pkg5 client smoke: native pkg(1) and pkgrecv against the running depot"
# Proves that the legacy Python client consumes what import-pkg5 publishes,
# served by the installed daemon under SMF as pkg6srv:
#   * the release .p5p and a dependency-free dummy package are imported with
#     /usr/bin/pkg6repo import-pkg5 into a scratch repository; re-importing
#     the release archive reports "already imported" and leaves every path
#     and checksum of the repository unchanged;
#   * negative control first: while the depot serves the operator repository
#     (which lacks ${pub}), a fresh pkg(1) image is created but cannot refresh
#     ${pub}, so the catalog check below cannot pass without the depot's
#     catalog route;
#   * the instance is switched to the scratch repository; a fresh scratch
#     image (pkg image-create --no-refresh, then refresh --full) must list
#     exactly the source FMRIs, which it can only learn from
#     /<pub>/catalog/1/, and pkg info -r must fetch the manifest;
#   * pkgrecv -s <depot> --newest lists the same FMRIs, and pkgrecv -a pulls
#     both packages over /manifest/0/ and /file/1/ into a scratch .p5p whose
#     manifests are byte-identical and payloads content-identical to the
#     sources (pkg5_smoke_compare);
#   * the dummy installs into the scratch image, its file matches the proto,
#     pkg verify passes, and it uninstalls again.
# pkg(1) and pkgrecv run with no_proxy for 127.0.0.1 (sp5/p5).
# Everything lives in one /var/tmp directory that is removed afterwards
# (also on failure); /var/pkg6depotd is not touched, and the instance is
# switched back to ${conf} before the upgrade checks.
# shellcheck source=pkg5-smoke-lib.sh
. "${here}/pkg5-smoke-lib.sh"
pub=${IPS_PUBLISHER}
url="http://127.0.0.1:${port}"
dummy=pkg6-release-test/pkg5-smoke
[ "${pub}" != test ] || fail "publisher '${pub}' collides with the operator repository's; the negative control needs another"
smoke=$(mktemp -d /var/tmp/pkg6-pkg5-smoke.XXXXXX)
case "${smoke}" in
  /var/tmp/pkg6-pkg5-smoke.?*) ;;
  *) fail "unexpected scratch directory '${smoke}'" ;;
esac
cleanup_smoke() { sudo rm -rf -- "${smoke}"; }
trap cleanup_smoke EXIT
chmod 0755 "${smoke}"
mkdir "${smoke}/tmp" "${smoke}/src"
chmod 0755 "${smoke}/src"
# p5 CMD...: run a pkg5 tool as the CI user with scratch temp space and no
# proxy between it and the depot.
p5() { TMPDIR="${smoke}/tmp" no_proxy=127.0.0.1 NO_PROXY=127.0.0.1 "$@"; }
# sp5 CMD...: the same as root. sudo resets the environment, so the no-proxy
# settings go through env(1) rather than sudoers' env_keep.
sp5() { sudo env TMPDIR="${smoke}/tmp" no_proxy=127.0.0.1 NO_PROXY=127.0.0.1 "$@"; }
# repo_tree: snapshot of the scratch repository (paths and file checksums).
repo_tree() {
  sudo bash -c '. "$1" && pkg5_smoke_tree "$2"' _ "${here}/pkg5-smoke-lib.sh" "${smoke}/depot/repo"
}
# depot_restart: restart the instance and wait until a new process answers.
depot_restart() {
  local old new=
  old=$(pgrep -u pkg6srv -f /usr/lib/pkg6depotd || true)
  smf svcadm restart "${svc}"
  for _ in $(seq 1 90); do
    new=$(pgrep -u pkg6srv -f /usr/lib/pkg6depotd || true)
    [ -n "${new}" ] && [ "${new}" != "${old}" ] && break
    sleep 1
  done
  [ -n "${new}" ] && [ "${new}" != "${old}" ] || fail "pkg6depotd did not restart (still '${old}')"
  wait_state online
  probe
}
serves_pub() { curl -sSf "${url}/${pub}/catalog/1/catalog.attrs" >/dev/null 2>&1; }

cp "${p5p}" "${smoke}/src/release.p5p"
mkdir -p "${smoke}/dummy-proto/opt/pkg6-pkg5-smoke"
echo "pkg6depotd release pkg5 client smoke ${smoke}" >"${smoke}/dummy-proto/opt/pkg6-pkg5-smoke/hello.txt"
cat >"${smoke}/dummy.p5m" <<EOF
set name=pkg.fmri value=${dummy}@1.0,5.11-0
set name=pkg.summary value="pkg6depotd release test: pkg5 client smoke"
dir path=opt owner=root group=bin mode=0755
dir path=opt/pkg6-pkg5-smoke owner=root group=bin mode=0755
file opt/pkg6-pkg5-smoke/hello.txt path=opt/pkg6-pkg5-smoke/hello.txt owner=root group=bin mode=0444
EOF
p5 pkgrepo create "${smoke}/dummy-repo"
p5 pkgrepo -s "${smoke}/dummy-repo" set "publisher/prefix=${pub}"
p5 pkgsend -s "${smoke}/dummy-repo" publish -d "${smoke}/dummy-proto" "${smoke}/dummy.p5m"
p5 pkgrecv -s "${smoke}/dummy-repo" -a -d "${smoke}/src/dummy.p5p" "${dummy}"
chmod 0644 "${smoke}/src/release.p5p" "${smoke}/src/dummy.p5p"
expected=$({ p5 pkgrecv -s "${smoke}/src/release.p5p" --newest && p5 pkgrecv -s "${smoke}/src/dummy.p5p" --newest; } | sort) \
  || fail "pkgrecv could not list the source archives"
rel_fmri=$(echo "${expected}" | grep -F "pkg://${pub}/${IPS_PKG_NAME}@" || true)
dummy_fmri=$(echo "${expected}" | grep -F "pkg://${pub}/${dummy}@" || true)
[ "$(echo "${rel_fmri}" | grep -c .)" = 1 ] || fail "release archive does not hold exactly one ${IPS_PKG_NAME} for ${pub}: ${expected}"
[ "$(echo "${dummy_fmri}" | grep -c .)" = 1 ] || fail "dummy archive does not hold ${dummy} for ${pub}: ${expected}"
echo "source FMRIs:"
echo "${expected}"

echo "--- import both archives into a scratch repository"
sudo mkdir -m 0750 "${smoke}/depot"
sudo chown pkg6srv:pkg6srv "${smoke}/depot"
sudo su pkg6srv -c "/usr/bin/pkg6repo create ${smoke}/depot/repo"
for a in release dummy; do
  sudo su pkg6srv -c "RUST_LOG=info /usr/bin/pkg6repo import-pkg5 -s ${smoke}/src/${a}.p5p -d ${smoke}/depot/repo" \
    || fail "import-pkg5 of the ${a} archive failed"
done
# Re-importing the same archive is a no-op: it must succeed, say so, and leave
# every path and byte of the repository as it was.
tree_before=$(repo_tree) || fail "cannot snapshot ${smoke}/depot/repo"
echo "${tree_before}" | grep -q '^f ' || fail "scratch repository holds no files after the import: ${tree_before}"
reimport_log=$(sudo su pkg6srv -c "RUST_LOG=info /usr/bin/pkg6repo import-pkg5 -s ${smoke}/src/release.p5p -d ${smoke}/depot/repo 2>&1") \
  || fail "re-importing the same archive failed: ${reimport_log}"
echo "${reimport_log}"
echo "${reimport_log}" | grep -qF 'archive already imported' \
  || fail "re-import did not report the archive as already imported"
tree_after=$(repo_tree) || fail "cannot snapshot ${smoke}/depot/repo"
[ "${tree_after}" = "${tree_before}" ] \
  || fail "re-import changed the repository: $(diff <(echo "${tree_before}") <(echo "${tree_after}") || true)"
echo "re-import left the repository unchanged ($(echo "${tree_before}" | grep -c .) entries)"
# Same config as the operator's, only the repository root differs.
sudo sed "s|^\([[:space:]]*root \)\"/var/pkg6depotd/repo\"|\1\"${smoke}/depot/repo\"|" "${conf}" \
  | sudo tee "${smoke}/depot.kdl" >/dev/null
sudo grep -qF "root \"${smoke}/depot/repo\"" "${smoke}/depot.kdl" || fail "could not point the scratch config at ${smoke}/depot/repo"
sudo chown root:pkg6srv "${smoke}/depot.kdl"
sudo chmod 0640 "${smoke}/depot.kdl"

echo "--- negative control: the operator repository does not serve ${pub}"
! serves_pub || fail "the operator repository already serves ${pub}; the catalog check would prove nothing"
# The image must be created, so that only the refresh can fail.
sp5 pkg image-create -F --no-refresh -p "${pub}=${url}/" "${smoke}/image-neg" \
  || fail "pkg image-create of the negative-control image failed"
if sp5 pkg -R "${smoke}/image-neg" refresh --full; then
  fail "pkg(1) refreshed ${pub} from a depot that does not serve it"
fi
echo "pkg(1) failed as expected without a served catalog for ${pub}"

echo "--- serve the scratch repository"
set_config_file "${smoke}/depot.kdl"
depot_restart
serves_pub || fail "depot does not serve ${url}/${pub}/catalog/1/catalog.attrs from the scratch repository"

echo "--- pkg(1): catalog and manifest in a scratch image"
img="${smoke}/image"
sp5 pkg image-create -F --no-refresh -p "${pub}=${url}/" "${img}" || fail "pkg image-create failed"
sp5 pkg -R "${img}" refresh --full || fail "pkg(1) could not read the catalog from ${url}"
got=$(sp5 pkg -R "${img}" list -Hafv | awk '{ print $1 }' | sort) || fail "pkg list -af found no packages in ${url}"
[ "${got}" = "${expected}" ] || fail "pkg(1) catalog lists '${got}', the sources hold '${expected}'"
sp5 pkg -R "${img}" info -r "${rel_fmri}" || fail "pkg info -r could not fetch the manifest of ${rel_fmri}"

echo "--- pkgrecv: manifests and payloads into a scratch archive"
got=$(p5 pkgrecv -s "${url}/" --newest | sort) || fail "pkgrecv could not list ${url}"
[ "${got}" = "${expected}" ] || fail "pkgrecv lists '${got}', the sources hold '${expected}'"
p5 pkgrecv -s "${url}/" -c "${smoke}/pkgrecv-cache" -a -d "${smoke}/received.p5p" "${rel_fmri}" "${dummy_fmri}" \
  || fail "pkgrecv could not retrieve the packages from ${url}"
for a in src/release src/dummy received; do
  pkg5_smoke_unpack "${smoke}/${a}.p5p" "${smoke}/unpacked-${a#src/}" || fail "cannot unpack ${smoke}/${a}.p5p"
done
diffs=$(pkg5_smoke_compare "${smoke}/unpacked-received" "${smoke}/unpacked-release" "${smoke}/unpacked-dummy") \
  || fail "pkgrecv copy from the depot differs from the sources: ${diffs}"
echo "manifests byte-identical, payloads content-identical:"
pkg5_smoke_list pkg "${smoke}/unpacked-received"

echo "--- pkg(1): install the dummy package into the scratch image"
sp5 pkg -R "${img}" install -v "${dummy_fmri}" || fail "pkg install of ${dummy_fmri} from ${url} failed"
[ "$(sp5 pkg -R "${img}" list -Hv "${dummy}" | awk '{ print $1 }')" = "${dummy_fmri}" ] \
  || fail "scratch image does not have ${dummy_fmri} installed"
sudo cmp "${img}/opt/pkg6-pkg5-smoke/hello.txt" "${smoke}/dummy-proto/opt/pkg6-pkg5-smoke/hello.txt" \
  || fail "installed dummy file differs from its proto"
sp5 pkg -R "${img}" verify -v "${dummy}" || fail "pkg verify of ${dummy} failed"
sp5 pkg -R "${img}" uninstall -v "${dummy}" || fail "pkg uninstall of ${dummy} failed"
sudo test ! -e "${img}/opt/pkg6-pkg5-smoke/hello.txt" || fail "dummy file still present after uninstall"

echo "--- back to the operator repository"
set_config_file "${conf}"
depot_restart
! serves_pub || fail "depot still serves ${pub} after switching back to ${conf}"
cleanup_smoke
trap - EXIT
[ ! -e "${smoke}" ] || fail "scratch directory ${smoke} was not removed"
echo "pkg5 client smoke passed"
pid1=$(pgrep -u pkg6srv -f /usr/lib/pkg6depotd)

echo "=== upgrade: operator config must survive"
# Same payload, higher branch, changed default config.
proto2="${work}/proto-upgrade"
rm -rf "${proto2}"
cp -R "${work}/proto" "${proto2}"
echo '// upgrade-test marker' >> "${proto2}/etc/pkg6depotd/pkg6depotd.kdl"
sed -E "s/^(set name=pkg\.fmri value=[^@]+@[^,]+,[0-9.]+-[0-9.]+)(:)/\1.1\2/" \
  "${work}/pkg6depotd.dep.res" > "${work}/upgrade.p5m"
new_fmri=$(sed -n 's/^set name=pkg\.fmri value=//p' "${work}/upgrade.p5m")
echo "upgrade target ${new_fmri}"
pkgsend -s "${work}/repo" publish -d "${proto2}" -d "${root}" "${work}/upgrade.p5m"
sudo pkg update -v -g "${work}/repo" "${new_fmri}"
new_ver=${new_fmri#*@}
new_ver=${new_ver%%:*}
pkg list -H -v "${IPS_PKG_NAME}" | grep -qF "@${new_ver}:" || fail "upgrade did not install ${new_fmri}"
sudo grep -q "127.0.0.1:${port}" "${conf}" || fail "operator change to ${conf} was lost on upgrade"
sudo grep -q 'upgrade-test marker' "${conf}.new" || fail "new default not delivered as ${conf}.new"
expect_attr "${conf}" "-rw-r----- root pkg6srv"
wait_state online
probe
pid2=$(pgrep -u pkg6srv -f /usr/lib/pkg6depotd)
echo "depot pid before upgrade ${pid1}, after ${pid2}"

echo "=== uninstall"
# Salvaging the operator-edited config (and its .kdl.new) into lost+found is
# expected pkg(1) behaviour for modified preserved files. What must never
# happen is anything from the repository location being salvaged or changed.
repo_salvage() { sudo find /var/pkg/lost+found -path '*var/pkg6depotd*' 2>/dev/null | sort; }
repo_before=$(sudo find /var/pkg6depotd -exec cksum {} + 2>/dev/null | sort; sudo find /var/pkg6depotd | sort)
salvage_before=$(repo_salvage)
sudo pkg uninstall -v "${IPS_PKG_NAME}"
[ -d /var/pkg6depotd/repo ] || fail "/var/pkg6depotd/repo is gone after uninstall"
[ "$(sudo find /var/pkg6depotd -exec cksum {} + 2>/dev/null | sort; sudo find /var/pkg6depotd | sort)" = "${repo_before}" ] \
  || fail "uninstall changed content under /var/pkg6depotd"
[ "$(repo_salvage)" = "${salvage_before}" ] \
  || fail "uninstall salvaged repository content into /var/pkg/lost+found: $(repo_salvage)"
echo "lost+found after uninstall (edited config expected):"
sudo find /var/pkg/lost+found -path '*etc/pkg6depotd*' 2>/dev/null || true
[ ! -e /usr/lib/pkg6depotd ] || fail "/usr/lib/pkg6depotd still present after uninstall"
for m in ${man_pages}; do
  [ ! -e "/usr/share/man/${m}" ] || fail "/usr/share/man/${m} still present after uninstall"
done
if svcs -H "${svc}" >/dev/null 2>&1; then
  s=$(svcs -H -o state "${svc}")
  [ "${s}" != online ] || fail "service still online after uninstall"
fi
pgrep -f /usr/lib/pkg6depotd >/dev/null && fail "pkg6depotd still running after uninstall"

echo "PASS: IPS package install/SMF/upgrade/uninstall"
