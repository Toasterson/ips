#!/usr/bin/env bash
# Self-tests for the illumos "Build test targets" steps in ci.yml and
# release.yml: the step scripts are cut out of the workflows verbatim and
# run against a fake cargo and stub host tools. Offline; runs on macOS
# (bash 3.2), Linux and illumos.
#
# Usage: .forgejo/scripts/tests/test-native-link-retry.sh
set -euo pipefail

root=$(cd "$(dirname "$0")/../../.." && pwd)

pass=0
failed=0
ok() { pass=$((pass + 1)); }
bad() { failed=$((failed + 1)); echo "not ok: $*" >&2; }
eq() { if [ "$2" = "$3" ]; then ok; else bad "$1: got '$2', want '$3'"; fi; }
has() { case "$2" in *"$3"*) ok ;; *) bad "$1: output lacks '$3'" ;; esac; }
lacks() { case "$2" in *"$3"*) bad "$1: output has '$3'" ;; *) ok ;; esac; }

tmp=$(mktemp -d)
trap 'rm -rf "${tmp}"' EXIT

# step_script WORKFLOW JOB STEP-NAME: print the step's `run: |` body, dedented.
step_script() {
  awk -v job="  $2:" -v step="      - name: $3" '
    $0 == job { injob = 1; next }
    injob && /^  [a-z]/ { injob = 0 }
    injob && $0 == step { instep = 1; next }
    instep && /^      - name:/ { exit }
    instep && /^        run: \|$/ { inrun = 1; next }
    inrun && /^          / { print substr($0, 11); next }
    inrun && /^ *$/ { print ""; next }
    inrun { exit }
  ' "$1"
}

# Stubs: cargo fails per FAKE_J2/FAKE_J1 (ok, link, compile) and logs its -j.
mkdir -p "${tmp}/bin"
cat > "${tmp}/bin/cargo" <<'EOF'
#!/bin/sh
for a in "$@"; do case "$a" in -j*) j="$a" ;; esac; done
echo "${j:-none}" >> "${FAKE_CALLS}"
case "$j" in -j2) mode="${FAKE_J2}" ;; -j1) mode="${FAKE_J1}" ;; *) mode=unexpected ;; esac
case "${mode}" in
  ok) echo "   Finished test profile"; exit 0 ;;
  link) printf '%s\n' "error: linking with \`gcc\` failed: exit status: 1" \
    "  = note: collect2: error: ld returned 1 exit status" >&2; exit 101 ;;
  compile) echo "error[E0308]: mismatched types" >&2; exit 101 ;;
  *) echo "fake cargo: unexpected jobs '${j:-none}'" >&2; exit 99 ;;
esac
EOF
for t in df swap kstat vmstat; do printf '#!/bin/sh\necho "stub %s"\n' "$t" > "${tmp}/bin/$t"; done
chmod +x "${tmp}/bin/"*

# run_step SCRIPT J2 J1 -> sets out, status, calls
run_step() {
  local dir="${tmp}/run.$$.$RANDOM"
  mkdir -p "${dir}"
  status=0
  out=$(cd "${dir}" && PATH="${tmp}/bin:${PATH}" FAKE_CALLS="${dir}/calls" FAKE_J2="$2" FAKE_J1="$3" \
    bash "$1" 2>&1) || status=$?
  calls=$(tr '\n' ' ' < "${dir}/calls" 2>/dev/null || true)
}

for wf in ci release; do
  file="${root}/.forgejo/workflows/${wf}.yml"
  case "${wf}" in
    ci) step="Build (test targets)" ;;
    release) step="Build test targets (release profile)" ;;
  esac
  script="${tmp}/${wf}.sh"
  step_script "${file}" illumos "${step}" > "${script}"
  if [ ! -s "${script}" ]; then bad "${wf}: step '${step}' not found"; continue; fi

  run_step "${script}" ok unused
  eq "${wf} -j2 pass: exit" "${status}" 0
  eq "${wf} -j2 pass: one build" "${calls}" "-j2 "
  lacks "${wf} -j2 pass: quiet" "${out}" "host state"

  run_step "${script}" link ok
  eq "${wf} link retry pass: exit" "${status}" 0
  eq "${wf} link retry pass: -j2 then -j1" "${calls}" "-j2 -j1 "
  has "${wf} link retry pass: host state" "${out}" "=== host state: build failed at -j2 ==="
  has "${wf} link retry pass: swap" "${out}" "stub swap"
  has "${wf} link retry pass: ARC" "${out}" "stub kstat"
  has "${wf} link retry pass: vmstat" "${out}" "stub vmstat"
  has "${wf} link retry pass: verdict" "${out}" "-j1 retry passed: the -j2 link failure was resource pressure"

  run_step "${script}" link link
  eq "${wf} link retry fail: exit" "${status}" 1
  eq "${wf} link retry fail: -j2 then -j1" "${calls}" "-j2 -j1 "
  has "${wf} link retry fail: second host state" "${out}" "=== host state: retry failed at -j1 ==="
  has "${wf} link retry fail: verdict" "${out}" "-j1 retry failed: reproducible link error"

  run_step "${script}" compile unused
  eq "${wf} compile error: exit" "${status}" 1
  eq "${wf} compile error: not retried" "${calls}" "-j2 "
  has "${wf} compile error: diagnostic kept" "${out}" "mismatched types"
done

# Every cargo build in the illumos jobs is capped; only the test-target
# steps retry, and nothing else uses -j1.
for wf in ci release; do
  body=$(awk '$0 == "  illumos:" { f = 1; next } f && /^  [a-z]/ { f = 0 } f' \
    "${root}/.forgejo/workflows/${wf}.yml" | grep -v '^ *#')
  uncapped=$(printf '%s\n' "${body}" | grep -E 'cargo (build|test) ' | grep -vE ' -j[12]( |;|$)' || true)
  eq "${wf}: illumos cargo builds carry -j" "${uncapped}" ""
  eq "${wf}: one -j1 retry" "$(printf '%s\n' "${body}" | grep -cE 'cargo (build|test) .* -j1')" 1
  eq "${wf}: nextest build capped" "$(printf '%s\n' "${body}" | grep -c 'export CARGO_BUILD_JOBS=2')" \
    "$(printf '%s\n' "${body}" | grep -cE 'cargo nextest (list|run)')"
done

echo "native link retry tests: ${pass} passed, ${failed} failed"
[ "${failed}" -eq 0 ]
