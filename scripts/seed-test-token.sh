#!/bin/sh
# Write a minimal geocoder.json auth DB with a fixed regression-test token.
#
# The server checks every request's `?key=` against the tokens map in
# geocoder.json. Tests need a deterministic token; this script writes
# one next to the index dir so `run-regression.sh` can use it.
#
# The hash under `users.regression.password_hash` is a placeholder —
# login flows are not exercised by tests. `rate_per_second: 0` and
# `rate_per_day: 0` mean unlimited.
#
# Usage:
#   ./scripts/seed-test-token.sh <data-dir>
set -eu

if [ $# -ne 1 ]; then
    echo "Usage: $0 <data-dir>" >&2
    exit 2
fi

DATA_DIR="$1"
if [ ! -d "$DATA_DIR" ]; then
    echo "Error: $DATA_DIR is not a directory" >&2
    exit 2
fi

AUTH_JSON="$DATA_DIR/geocoder.json"
TOKEN="${REGRESSION_TEST_TOKEN:-REGRESSION_TEST_TOKEN}"

cat > "$AUTH_JSON" <<EOF
{
  "users": {
    "regression": {
      "password_hash": "\$2b\$12\$placeholder.never.used.for.login",
      "admin": false,
      "rate_per_second": 0,
      "rate_per_day": 0,
      "rate_by_ip": false
    }
  },
  "tokens": {
    "$TOKEN": "regression"
  }
}
EOF

echo "seeded $AUTH_JSON with token '$TOKEN'"
