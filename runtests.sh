#!/bin/bash
# Rust-only regression driver (pgmicro / tursodb engine).
#
# Does NOT run turso_sqlite3 C-compat tests (see `make test-sqlite3`).
#
# Usage:
#   ./runtests.sh                         # pgmicro-focused run + fuzz (single-threaded)
#   QUICK=1 ./runtests.sh                 # skip fuzz (~30+ min saved)
#   FUZZ_TIMEOUT=120 ./runtests.sh        # cap fuzz phase at 120 seconds
#   FUZZ_FILTER=affinity_fuzz ./runtests.sh
#   FULL=1 ./runtests.sh                  # also run entire workspace (slow; many crates)
#
# Environment:
#   QUICK=1          Skip fuzz_tests
#   FUZZ_TIMEOUT=N   Stop fuzz after N seconds (exit 124 = timed out, partial OK)
#   FUZZ_FILTER=...  Filter for fuzz_tests binary
#   FULL=1           Run `cargo test --workspace` before focused tests
#   RUST_LOG=...     Forwarded to cargo test

set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

mkdir -p test-logs
LOG="test-logs/full-regression-$(date +%Y%m%d-%H%M%S).log"

WORKSPACE_EXCLUDES=(
  --exclude memory-benchmark
  --exclude turso_sqlite3
  --exclude core_tester
)

status=0
note() { echo "$*" | tee -a "$LOG"; }

# Run a command, log output, record failure but keep going.
run_logged() {
  note ">>> $*"
  set +e
  "$@" 2>&1 | tee -a "$LOG"
  local rc=${PIPESTATUS[0]}
  set -e
  if [[ $rc -ne 0 ]]; then
    status=$rc
    note ">>> failed (exit $rc): $*"
  fi
}

note "=== runtests.sh started $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
note "QUICK=${QUICK:-} FUZZ_TIMEOUT=${FUZZ_TIMEOUT:-} FUZZ_FILTER=${FUZZ_FILTER:-} FULL=${FULL:-}"

if [[ -n "${FULL:-}" ]]; then
  note ">>> FULL=1: running entire workspace (excluding memory-benchmark, turso_sqlite3, core_tester)"
  run_logged cargo test --workspace \
    "${WORKSPACE_EXCLUDES[@]}" \
    --no-fail-fast
fi

# core_tester lib — prepare_execute_batch, COUNT regressions, pragma tests
run_logged cargo test -p core_tester --lib --no-fail-fast

# Postgres integration + engine API tests
run_logged cargo test -p core_tester --test integration_tests integration::postgres --no-fail-fast

# pgmicro CLI (matches pgmicro-ci.yml)
run_logged cargo test -p pgmicro --no-fail-fast

# PG parser / translator
run_logged cargo test -p turso_parser_pg --no-fail-fast

# Fuzz differential oracle — single-threaded avoids rusqlite CannotOpen flakes
if [[ -z "${QUICK:-}" ]]; then
  fuzz_cmd=(cargo test -p core_tester --test fuzz_tests --no-fail-fast -- --test-threads=1)
  if [[ -n "${FUZZ_FILTER:-}" ]]; then
    fuzz_cmd+=("$FUZZ_FILTER")
  fi

  if [[ -n "${FUZZ_TIMEOUT:-}" ]]; then
    note ">>> fuzz phase (timeout ${FUZZ_TIMEOUT}s): ${fuzz_cmd[*]}"
    set +e
    timeout "$FUZZ_TIMEOUT" "${fuzz_cmd[@]}" 2>&1 | tee -a "$LOG"
    fuzz_rc=${PIPESTATUS[0]}
    set -e
    if [[ $fuzz_rc -eq 124 ]]; then
      note ">>> fuzz stopped after ${FUZZ_TIMEOUT}s (timeout — partial run)"
    elif [[ $fuzz_rc -ne 0 ]]; then
      status=$fuzz_rc
      note ">>> fuzz failed (exit $fuzz_rc)"
    fi
  else
    run_logged "${fuzz_cmd[@]}"
  fi
else
  note ">>> skipping fuzz (QUICK=1)"
fi

note "=== runtests.sh finished exit=$status $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
note "log: $LOG"
exit "$status"