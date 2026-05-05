#!/usr/bin/env bash
# Smoke test harness for nico-doctor + nico-correlate against a live cluster.
# Runs in under 2 minutes; exits 0 on pass, 1 on failure.
#
# Usage: ./scripts/smoke.sh [--env <path>]   # defaults to .env.local

set -euo pipefail

ENV_FILE=".env.local"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --env) ENV_FILE="$2"; shift 2 ;;
        *) echo "usage: $0 [--env <path>]" >&2; exit 1 ;;
    esac
done

# ── 1. Load env file ────────────────────────────────────────────────────────
if [[ ! -f "$ENV_FILE" ]]; then
    echo "error: env file not found: $ENV_FILE" >&2
    echo "  hint: cp .env.example .env.local  then fill in your values" >&2
    exit 1
fi

# shellcheck source=/dev/null
source "$ENV_FILE"

# ── 2. Build or locate binaries ─────────────────────────────────────────────
DOCTOR_BIN=""
CORRELATE_BIN=""

if [[ -n "${NICO_BIN_DIR:-}" ]]; then
    DOCTOR_BIN="$NICO_BIN_DIR/nico-doctor"
    CORRELATE_BIN="$NICO_BIN_DIR/nico-correlate"
    if [[ ! -x "$DOCTOR_BIN" || ! -x "$CORRELATE_BIN" ]]; then
        echo "error: binaries not found in NICO_BIN_DIR=$NICO_BIN_DIR" >&2
        exit 1
    fi
    echo "smoke: using pre-built binaries from $NICO_BIN_DIR"
else
    echo "smoke: building release binaries..."
    BUILD_START=$(date +%s)
    cargo build --release -p nico-doctor -p nico-correlate 2>&1
    BUILD_END=$(date +%s)
    echo "smoke: build done in $(( BUILD_END - BUILD_START ))s"
    DOCTOR_BIN="target/release/nico-doctor"
    CORRELATE_BIN="target/release/nico-correlate"
fi

# ── helpers ──────────────────────────────────────────────────────────────────
PASS=0
FAIL=0
RESULTS=()

check() {
    local name="$1"; shift
    local start end elapsed rc
    start=$(date +%s)
    # Capture both stdout+stderr but don't abort on non-zero exit.
    set +e
    out=$("$@" 2>&1)
    rc=$?
    set -e
    end=$(date +%s)
    elapsed=$(( end - start ))
    RESULTS+=("$name:$rc:${elapsed}s")
    echo "$out"
    return $rc
}

pass() { echo "  PASS  $1"; (( PASS++ )) || true; }
fail() { echo "  FAIL  $1  ($2)"; (( FAIL++ )) || true; }

echo ""
echo "══════════════════════════════════════════════════════"
echo " nico smoke test"
echo "══════════════════════════════════════════════════════"
echo ""

TOTAL_START=$(date +%s)

# ── 3. nico-doctor: exit code must be 0, 1, or 2 (not 3 = unknown error) ────
echo "▶ nico-doctor (full run)"
set +e
check "nico-doctor" "$DOCTOR_BIN" 2>&1
DOCTOR_RC=$?
set -e

if [[ $DOCTOR_RC -eq 0 || $DOCTOR_RC -eq 1 || $DOCTOR_RC -eq 2 ]]; then
    pass "nico-doctor exit code $DOCTOR_RC (ok/warn/fail — not unknown)"
else
    fail "nico-doctor" "exit code $DOCTOR_RC (expected 0, 1, or 2; got $DOCTOR_RC — suggests internal error)"
fi

# ── 4. nico-correlate: exit code must be 0 or 2 (not 1 = ID not found) ──────
if [[ -z "${SMOKE_WORKFLOW_ID:-}" || "$SMOKE_WORKFLOW_ID" == "<a known recent workflow ID>" ]]; then
    echo ""
    echo "  SKIP  nico-correlate (SMOKE_WORKFLOW_ID not set)"
else
    echo ""
    echo "▶ nico-correlate $SMOKE_WORKFLOW_ID"
    set +e
    check "nico-correlate" "$CORRELATE_BIN" "$SMOKE_WORKFLOW_ID" 2>&1
    CORRELATE_RC=$?
    set -e

    if [[ $CORRELATE_RC -eq 0 || $CORRELATE_RC -eq 2 ]]; then
        pass "nico-correlate exit code $CORRELATE_RC (found or no-data)"
    elif [[ $CORRELATE_RC -eq 1 ]]; then
        fail "nico-correlate" "exit code 1 means the workflow ID was not found — check SMOKE_WORKFLOW_ID"
    else
        fail "nico-correlate" "exit code $CORRELATE_RC (unexpected)"
    fi
fi

# ── 5. nico-doctor --json | jq . ─────────────────────────────────────────────
echo ""
echo "▶ nico-doctor --json (JSON parse check)"
if ! command -v jq &>/dev/null; then
    echo "  SKIP  jq not installed — skipping JSON parse check"
else
    set +e
    JSON_OUT=$("$DOCTOR_BIN" --json 2>/dev/null)
    JSON_RC=$?
    set -e

    if echo "$JSON_OUT" | jq . >/dev/null 2>&1; then
        pass "nico-doctor --json produces valid JSON (exit $JSON_RC)"
    else
        fail "nico-doctor --json" "output is not valid JSON"
        echo "--- raw output ---"
        echo "$JSON_OUT"
        echo "------------------"
    fi
fi

# ── Summary ──────────────────────────────────────────────────────────────────
TOTAL_END=$(date +%s)
TOTAL_ELAPSED=$(( TOTAL_END - TOTAL_START ))

echo ""
echo "══════════════════════════════════════════════════════"
if [[ $FAIL -eq 0 ]]; then
    echo " PASSED  ($PASS checks, ${TOTAL_ELAPSED}s)"
    echo "══════════════════════════════════════════════════════"
    exit 0
else
    echo " FAILED  ($FAIL/$((PASS + FAIL)) checks failed, ${TOTAL_ELAPSED}s)"
    echo "══════════════════════════════════════════════════════"
    exit 1
fi
