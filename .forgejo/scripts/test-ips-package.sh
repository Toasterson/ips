#!/usr/bin/env bash
# Install, run, upgrade and remove package/pkg6/depotd on the LIVE image of
# the machine it runs on, and assert OpenIndiana service behaviour:
#   * the pkg6srv account (UID/GID 94) and file ownership/modes
#   * the SMF manifest validates and imports with the instance disabled
#   * the service starts as pkg6srv and answers /versions/0/
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
if svcs -H "${svc}" >/dev/null 2>&1; then
  s=$(svcs -H -o state "${svc}")
  [ "${s}" != online ] || fail "service still online after uninstall"
fi
pgrep -f /usr/lib/pkg6depotd >/dev/null && fail "pkg6depotd still running after uninstall"

echo "PASS: IPS package install/SMF/upgrade/uninstall"
