# shellcheck shell=bash
# Helpers for the pkg5 client smokes: the root-only live-image one in
# test-ips-package.sh (release CI) and the unprivileged pre-tag one in
# test-pkg5-unprivileged.sh. They only touch the directories passed to them,
# so tests/test-release-scripts.sh exercises them offline. bash 3.2
# compatible.

# pkg5_smoke_unpack P5P DIR: unpack a .p5p (a pax tar) into DIR, which must
# not exist yet. Prefers GNU tar where installed.
pkg5_smoke_unpack() {
  local archive="$1" dir="$2" tar=tar
  case "${archive}" in /*) ;; *) archive="$(pwd)/${archive}" ;; esac
  command -v gtar >/dev/null 2>&1 && tar=gtar
  mkdir "${dir}" && (cd "${dir}" && "${tar}" -xf "${archive}")
}

# pkg5_smoke_list KIND DIR: print the manifests (KIND=pkg) or payloads
# (KIND=file) of an unpacked p5p, relative to DIR, sorted.
pkg5_smoke_list() {
  (cd "$2" 2>/dev/null && find publisher -path "publisher/*/$1/*" -type f 2>/dev/null | sort)
}

# pkg5_smoke_compare GOT SRC...: compare an unpacked p5p received from the
# depot (GOT) with the unpacked source archives (SRC...).
#   * every source manifest is in GOT at the same path, byte for byte;
#   * every source payload is in GOT at the same path, both are gzip, and
#     both decompress to the same bytes (the depot may recompress);
#   * GOT holds exactly as many manifests as the sources together;
#   * each source holds at least one manifest and one payload, so the
#     comparison can never pass vacuously.
# Prints one line per problem and returns 1 if there is any.
pkg5_smoke_compare() {
  local got="$1" src rel manifests payloads n_src=0 n_got bad=0
  shift
  [ "$#" -gt 0 ] || { echo "no source archives to compare with"; return 1; }
  for src in "$@"; do
    manifests=$(pkg5_smoke_list pkg "${src}")
    payloads=$(pkg5_smoke_list file "${src}")
    [ -n "${manifests}" ] || { echo "${src}: holds no manifest"; bad=1; }
    [ -n "${payloads}" ] || { echo "${src}: holds no payload"; bad=1; }
    for rel in ${manifests}; do
      n_src=$((n_src + 1))
      if [ ! -f "${got}/${rel}" ]; then
        echo "manifest missing from the depot copy: ${rel}"; bad=1
      elif ! cmp -s "${src}/${rel}" "${got}/${rel}"; then
        echo "manifest differs from the source: ${rel}"; bad=1
      fi
    done
    for rel in ${payloads}; do
      if [ ! -f "${got}/${rel}" ]; then
        echo "payload missing from the depot copy: ${rel}"; bad=1
      elif ! gzip -t "${src}/${rel}" 2>/dev/null || ! gzip -t "${got}/${rel}" 2>/dev/null; then
        echo "payload is not gzip: ${rel}"; bad=1
      elif ! cmp -s <(gzip -dc <"${src}/${rel}") <(gzip -dc <"${got}/${rel}"); then
        echo "payload differs from the source: ${rel}"; bad=1
      fi
    done
  done
  n_got=$(pkg5_smoke_list pkg "${got}" | grep -c . || true)
  [ "${n_got}" = "${n_src}" ] || { echo "depot copy holds ${n_got} manifests, the sources ${n_src}"; bad=1; }
  return "${bad}"
}

# pkg5_smoke_tree DIR: print one sorted line per entry below DIR (type,
# relative path, and the cksum of a file or the target of a symlink), so two
# snapshots are equal exactly when the tree and every file's bytes are.
# Fails if DIR cannot be entered.
pkg5_smoke_tree() {
  (cd "$1" || exit 1
   find . -print | LC_ALL=C sort | while IFS= read -r p; do
     if [ -h "${p}" ]; then echo "l ${p} -> $(readlink "${p}")"
     elif [ -f "${p}" ]; then echo "f ${p} $(cksum <"${p}")"
     elif [ -d "${p}" ]; then echo "d ${p}"
     else echo "? ${p}"
     fi
   done)
}
