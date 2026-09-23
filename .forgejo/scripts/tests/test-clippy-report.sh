#!/usr/bin/env bash
# Self-tests for .forgejo/scripts/clippy-report.sh against tests/fake-cargo.
# Offline; runs on macOS (bash 3.2), Linux and illumos. Needs jq.
#
# Usage: .forgejo/scripts/tests/test-clippy-report.sh
set -euo pipefail

tests=$(cd "$(dirname "$0")" && pwd)
report="${tests}/../clippy-report.sh"

pass=0
failed=0
ok() { pass=$((pass + 1)); }
bad() { failed=$((failed + 1)); echo "not ok: $*" >&2; }
eq() { if [ "$2" = "$3" ]; then ok; else bad "$1: got '$2', want '$3'"; fi; }
has() { case "$2" in *"$3"*) ok ;; *) bad "$1: output lacks '$3'" ;; esac; }
lacks() { case "$2" in *"$3"*) bad "$1: output has '$3'" ;; *) ok ;; esac; }

tmp=$(mktemp -d)
trap 'rm -rf "${tmp}"' EXIT
run() { # run MODE [ENV...] -> sets out, status
  local mode="$1"
  shift
  status=0
  out=$(env CARGO="${tests}/fake-cargo" CLIPPY_REPORT_DIR="${tmp}/$mode" FAKE_CARGO_MODE="${mode}" "$@" \
    "${report}" --workspace --all-targets --locked 2>&1) || status=$?
}

# Warnings only: exit 0, deduped counts per lint, no snippets, no stderr dump.
# With the default 60 s heartbeat this must still return at once: a leftover
# heartbeat sleep holding stdout would stall the CI step.
SECONDS=0
run warnings
[ "${SECONDS}" -lt 10 ] && ok || bad "report returned after ${SECONDS}s; heartbeat left running"
eq "warnings exit" "${status}" 0
has "warnings summary" "${out}" "exit status 0; 3 unique warnings; 0 error diagnostics"
has "per-lint count" "${out}" "   3 clippy::collapsible_if"
has "one-line warning" "${out}" "libips/src/f2.rs:2: clippy::collapsible_if: this \`if\` can be collapsed"
lacks "no rendered warning bodies" "${out}" "warning: this"
lacks "no stderr dump on success" "${out}" "cargo stderr"

# The one-line list is capped; the per-lint total is not.
run warnings FAKE_CARGO_WARNINGS=50 CLIPPY_WARNING_LINES=5
eq "capped exit" "${status}" 0
has "capped total" "${out}" "50 unique warnings"
eq "capped list length" "$(printf '%s\n' "${out}" | grep -c ': clippy::collapsible_if: ')" 5

# Compile error: exit status preserved, error printed in full, stderr tail shown.
run error
eq "error exit" "${status}" 101
has "error summary" "${out}" "exit status 101; 1 unique warnings; 1 error diagnostics"
has "full error body" "${out}" "FULL-ERROR-BODY"
has "cargo stderr on failure" "${out}" "could not compile \`libips\`"

# Cargo-level failure with no diagnostics still fails and says why.
run locked
eq "locked exit" "${status}" 101
has "locked stderr" "${out}" "--locked was passed"

# Heartbeat fires while clippy runs.
run warnings FAKE_CARGO_SLEEP=3 CLIPPY_HEARTBEAT_SECS=1
eq "heartbeat exit" "${status}" 0
has "heartbeat" "${out}" "clippy: still running after"

echo "clippy report tests: ${pass} passed, ${failed} failed"
[ "${failed}" -eq 0 ]
