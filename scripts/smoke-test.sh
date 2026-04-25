#!/usr/bin/env bash
# Post-deploy smoke test for the query server.
#
# Hits a known reverse-geocode coord and asserts the country code,
# then verifies /healthz/ready returns 200. Suitable for a deploy
# pipeline's "post-deploy verify" step or a manual sanity check
# after rolling a new AMI / pod.
#
# Exit codes:
#   0 — all checks passed
#   1 — service not reachable, request failed, or assertion failed
#
# Usage:
#   scripts/smoke-test.sh [base_url] [expected_country] [lat] [lon]
#
# Defaults to http://localhost:3000 and Sydney Opera House
# (-33.8568, 151.2153) → AU. Pin a different coord per deployment if
# you serve different countries.

set -euo pipefail

BASE_URL="${1:-${GEOCODER_URL:-http://localhost:3000}}"
EXPECTED_CC="${2:-${SMOKE_EXPECTED_CC:-au}}"
LAT="${3:-${SMOKE_LAT:--33.8568}}"
LON="${4:-${SMOKE_LON:-151.2153}}"
TIMEOUT="${SMOKE_TIMEOUT_SEC:-5}"

red()    { printf '\033[31m%s\033[0m\n' "$1"; }
green()  { printf '\033[32m%s\033[0m\n' "$1"; }
yellow() { printf '\033[33m%s\033[0m\n' "$1"; }

fail() {
    red "FAIL: $1"
    exit 1
}

# --- 1. /healthz/live should return 200 immediately -------------------

if ! curl -fsS --max-time "$TIMEOUT" "$BASE_URL/healthz/live" >/dev/null; then
    fail "/healthz/live not responding 200 at $BASE_URL"
fi
green "OK   /healthz/live"

# --- 2. /healthz/ready must return 200 (Index::load completed) --------
#
# An instance still on the loading path returns 503; that's normal
# for the first few seconds of boot. We retry with backoff for up to
# 60s, after which we give up and fail.

ready=""
for attempt in 1 2 3 4 5 6 7 8 9 10; do
    code=$(curl -s --max-time "$TIMEOUT" -o /dev/null -w '%{http_code}' "$BASE_URL/healthz/ready")
    if [[ "$code" == "200" ]]; then
        ready=ok
        break
    fi
    yellow "WAIT /healthz/ready returned $code (attempt $attempt/10)"
    sleep 6
done
[[ -n "$ready" ]] || fail "/healthz/ready never returned 200 after 60s"
green "OK   /healthz/ready"

# --- 3. /reverse with a known coord must return the expected country --
#
# Uses jq when available for clean parsing; falls back to grep for
# environments that don't ship jq (alpine-based containers, etc.).

resp=$(curl -fsS --max-time "$TIMEOUT" "$BASE_URL/reverse?lat=$LAT&lon=$LON") \
    || fail "/reverse?lat=$LAT&lon=$LON did not return 200"

if command -v jq >/dev/null 2>&1; then
    got_cc=$(printf '%s' "$resp" | jq -r '.address.country_code // empty')
else
    got_cc=$(printf '%s' "$resp" | grep -oE '"country_code":"[a-z]{2}"' | head -1 | cut -d'"' -f4 || true)
fi

if [[ -z "$got_cc" ]]; then
    red "FAIL: no country_code in response. Body:"
    printf '%s\n' "$resp" | head -c 500
    exit 1
fi

# Case-fold both sides — operators sometimes pass uppercase. We use
# tr rather than bash 4's ${var,,} so macOS' system bash 3.2 (still
# the default in CI on macOS runners) doesn't choke.
got_cc_lower=$(printf '%s' "$got_cc" | tr '[:upper:]' '[:lower:]')
expected_cc_lower=$(printf '%s' "$EXPECTED_CC" | tr '[:upper:]' '[:lower:]')
if [[ "$got_cc_lower" != "$expected_cc_lower" ]]; then
    fail "expected country_code=$expected_cc_lower for ($LAT, $LON), got '$got_cc_lower'"
fi
green "OK   /reverse?lat=$LAT&lon=$LON → country_code=$got_cc_lower"

# --- 4. /metrics scrape must return 200 + Prometheus content-type ----

ct=$(curl -s --max-time "$TIMEOUT" -I "$BASE_URL/metrics" | grep -i '^content-type:' | head -1 | tr -d '\r')
if [[ -z "$ct" ]] || [[ "$ct" != *"text/plain"* ]]; then
    fail "/metrics did not return text/plain content-type (got: ${ct:-empty})"
fi
green "OK   /metrics scrape"

green "PASS smoke-test against $BASE_URL"
