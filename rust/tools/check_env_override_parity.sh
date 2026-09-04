#!/usr/bin/env bash
# Verify the Rust _apply_env_overrides port references every env var the Python
# original does. Python source: gateway/config.py lines 2042-end.
# Usage: rust/tools/check_env_override_parity.sh
set -uo pipefail
REPO=/home/eins0fx/development/hermes-agent-port
PY="$REPO/gateway/config.py"
RS="$REPO/rust/crates/hermes-gateway/src/config_env_overrides.rs"

if [ ! -f "$RS" ]; then echo "missing $RS"; exit 2; fi

start=$(grep -n '^def _apply_env_overrides' "$PY" | cut -d: -f1)
awk -v s="$start" 'NR>=s' "$PY" | grep -oE '"[A-Z][A-Z0-9_]{2,}"' | tr -d '"' | sort -u > /tmp/py_envvars.txt
grep -oE '"[A-Z][A-Z0-9_]{2,}"' "$RS" | tr -d '"' | sort -u > /tmp/rs_envvars.txt

echo "python env vars: $(wc -l < /tmp/py_envvars.txt)"
echo "rust   env vars: $(wc -l < /tmp/rs_envvars.txt)"
missing=$(comm -23 /tmp/py_envvars.txt /tmp/rs_envvars.txt)
if [ -n "$missing" ]; then
  echo "--- MISSING FROM RUST ($(echo "$missing" | wc -l)) ---"
  echo "$missing"
  exit 1
fi
echo "OK: every Python env var is referenced in the Rust port"
extra=$(comm -13 /tmp/py_envvars.txt /tmp/rs_envvars.txt)
[ -n "$extra" ] && { echo "--- extra in rust (review, may be enum/consts) ---"; echo "$extra"; }
exit 0
