#!/usr/bin/env bash
# Report-only clippy for .forgejo/workflows/ci.yml with a bounded log.
#
# Streaming every clippy warning (hundreds, with source snippets) into the
# job log made it large and slow to scroll. This runs clippy with its
# diagnostics captured as JSON under $CLIPPY_REPORT_DIR and prints instead:
#   * a heartbeat every $CLIPPY_HEARTBEAT_SECS while clippy runs,
#   * warning counts per lint and a one-line-per-warning list (capped),
#   * every error diagnostic in full, never capped,
#   * on failure, the tail of cargo's stderr (cargo-level errors such as a
#     stale Cargo.lock under --locked, build script failures, ICEs).
# The exit status is clippy's own, so the step still fails on any error.
#
# Usage: .forgejo/scripts/clippy-report.sh [CLIPPY_ARGS...]
#   e.g. clippy-report.sh --workspace --all-targets --all-features --locked
# Env: CARGO (default cargo), CLIPPY_REPORT_DIR (default target/clippy-report),
#   CLIPPY_HEARTBEAT_SECS (default 60), CLIPPY_WARNING_LINES (default 300),
#   CLIPPY_STDERR_LINES (default 200). Needs jq.
set -euo pipefail

cargo_cmd="${CARGO:-cargo}"
dir="${CLIPPY_REPORT_DIR:-target/clippy-report}"
heartbeat="${CLIPPY_HEARTBEAT_SECS:-60}"
max_warnings="${CLIPPY_WARNING_LINES:-300}"
max_stderr="${CLIPPY_STDERR_LINES:-200}"

mkdir -p "${dir}"
json="${dir}/clippy.json"
stderr="${dir}/clippy.stderr"
diags="${dir}/diagnostics.json"

echo "clippy: ${cargo_cmd} clippy $* --message-format=json (diagnostics in ${json})"
"${cargo_cmd}" clippy "$@" --message-format=json >"${json}" 2>"${stderr}" &
pid=$!
# The heartbeat's sleep must die with it: an orphaned sleep would hold the
# step's stdout open until it wakes.
(
  nap=""
  trap '[ -z "${nap}" ] || kill "${nap}" 2>/dev/null; exit 0' TERM
  start=$(date +%s)
  while :; do
    sleep "${heartbeat}" &
    nap=$!
    wait "${nap}"
    kill -0 "${pid}" 2>/dev/null || exit 0
    echo "clippy: still running after $(($(date +%s) - start))s; last cargo line: $(tail -n 1 "${stderr}" 2>/dev/null | cut -c1-160)"
  done
) &
beat=$!
status=0
wait "${pid}" || status=$?
kill "${beat}" 2>/dev/null || true
wait "${beat}" 2>/dev/null || true

# One object per rustc diagnostic. Cargo reports a diagnostic once per target
# that compiles the file (lib and test under --all-targets), so dedupe on the
# rendered text. Non-JSON lines on stdout are skipped, not fatal.
jq -R -c 'fromjson? | select(.reason == "compiler-message") | .message' "${json}" \
  | jq -s -c 'unique_by(.rendered) | .[]' >"${diags}" || {
  echo "clippy: could not parse ${json}; showing its tail instead" >&2
  tail -n "${max_stderr}" "${json}" | cut -c1-400
}

# rustc's own "N warnings emitted" / "aborting due to" summaries carry no
# spans and no code; they are not findings.
finding='select((.spans | length) > 0 or .code != null)'
nwarn=$(jq -s "[.[] | select(.level == \"warning\") | ${finding}] | length" "${diags}" 2>/dev/null || echo 0)
nerr=$(jq -s '[.[] | select(.level == "error" or .level == "error: internal compiler error")] | length' "${diags}" 2>/dev/null || echo 0)

echo "clippy: exit status ${status}; ${nwarn} unique warnings; ${nerr} error diagnostics"

if [ "${nwarn}" -gt 0 ]; then
  echo "=== warnings by lint ==="
  jq -r "select(.level == \"warning\") | ${finding} | .code.code // \"(no code)\"" "${diags}" \
    | sort | uniq -c | sort -rn
  echo "=== warnings (first ${max_warnings}) ==="
  jq -r "select(.level == \"warning\") | ${finding}
    | ((.spans[] | select(.is_primary)) // .spans[0] // {}) as \$s
    | \"\(\$s.file_name // \"?\"):\(\$s.line_start // \"?\"): \(.code.code // \"-\"): \(.message)\"" "${diags}" \
    | sort -u | head -n "${max_warnings}" | cut -c1-300
fi

if [ "${nerr}" -gt 0 ]; then
  echo "=== errors (all, full) ==="
  jq -r 'select(.level == "error" or .level == "error: internal compiler error") | .rendered' "${diags}"
fi

if [ "${status}" -ne 0 ]; then
  echo "=== cargo stderr (last ${max_stderr} lines) ==="
  tail -n "${max_stderr}" "${stderr}"
fi

exit "${status}"
