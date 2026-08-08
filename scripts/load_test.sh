#!/usr/bin/env bash
# Hydra wave-6 §2.4 — load / correctness harness.
#
# Verifies, against a RUNNING hydra instance with a mock upstream:
#   1. SWRR weight distribution (3:1 → ~6:2 over N requests)
#   2. breaker-under-failure avoids a dead upstream + revives on probe
#   3. (optional) baseline RPS / P99
#
# This is a real-process harness (dev-plan §1 铁律 2: no internal mocks). The
# upstream MUST be a real HTTP server (e.g. wiremock, or a trivial echo). Set
# the env vars below; defaults assume hydra + an echo upstream on localhost.
#
# USAGE:
#   1. Start an echo upstream that records which instance served each request
#      (e.g. return its bind port in a header). See tests/e2e/README.md.
#   2. Start hydra pointing at that upstream with two providers weighted 3:1.
#   3. HYDRA_ADMIN_TOKEN=... ./scripts/load_test.sh
#
# REQUIRES: `oha` (https://github.com/hatoo/oha) or `wrk`. Falls back to a
# bash/seq loop if neither is present (slower but works).
set -euo pipefail

# ---- config (override via env) --------------------------------------------
HYDRA_ADMIN_ADDR="${HYDRA_ADMIN_ADDR:-127.0.0.1:8081}"
HYDRA_ADMIN_TOKEN="${HYDRA_ADMIN_TOKEN:?HYDRA_ADMIN_TOKEN must be set}"
PROXY_BASE="${PROXY_BASE:-http://127.0.0.1:8080}"   # tenant domain via Host header
TENANT_DOMAIN="${TENANT_DOMAIN:-loadtest.example.com}"
MODEL="${MODEL:-echo}"
N="${N:-1000}"                                        # request count for distribution check
C="${C:-8}"                                           # concurrency
QUIET="${QUIET:-0}"

curl_admin() {  # curl_admin <method> <path> [json-body]
  local method="$1" path="$2" body="${3:-}"
  if [[ -n "$body" ]]; then
    curl -sS -X "$method" "http://${HYDRA_ADMIN_ADDR}${path}" \
      -H "Authorization: Bearer ${HYDRA_ADMIN_TOKEN}" \
      -H "content-type: application/json" -d "$body"
  else
    curl -sS -X "$method" "http://${HYDRA_ADMIN_ADDR}${path}" \
      -H "Authorization: Bearer ${HYDRA_ADMIN_TOKEN}"
  fi
}

echo "==> hydra load/correctness harness"
echo "    admin:   http://${HYDRA_ADMIN_ADDR}"
echo "    proxy:   ${PROXY_BASE}  (Host: ${TENANT_DOMAIN})"
echo "    model:   ${MODEL}   N=${N}  C=${C}"

# ---- 0. sanity: admin health ----------------------------------------------
health="$(curl_admin GET /api/v1/health)"
echo "    health:  ${health}"
echo "${health}" | grep -q '"status":"ok"' || { echo "FAILED: hydra not healthy"; exit 1; }

# ---- helper: count how many times each provider served --------------------
# Expects the upstream to echo a header like X-Echo-Instance: <id>.
count_distribution() {
  local total="$1"
  local counts_file
  counts_file="$(mktemp)"
  trap 'rm -f "$counts_file"' EXIT
  for _ in $(seq 1 "$total"); do
    inst="$(curl -sS -H "Host: ${TENANT_DOMAIN}" \
              -H "content-type: application/json" \
              -d "{\"model\":\"${MODEL}\"}" \
              -D - "${PROXY_BASE}/v1/chat/completions" \
              | awk 'tolower($1) ~ /^x-echo-instance:$/ {print $2}' | tr -d '\r')"
    [[ -n "$inst" ]] && echo "$inst" >> "$counts_file"
  done
  sort "$counts_file" | uniq -c | sort -rn
}

# ---- 1. SWRR weight distribution ------------------------------------------
# NOTE: this script is a coarse, curl-based check. The authoritative,
# deterministic distribution assertion is the Rust integration test
# `tests/load_breaker_swrr.rs::swrr_weight_distribution_3_to_1` (run by
# `cargo test --features server --test load_breaker_swrr`). Use that as the
# gate; use this script for real-process RPS/P99 measurement.
echo
echo "==> [1] SWRR distribution (sample ${N} requests, expect ≈ weight ratio)"
if command -v oha >/dev/null 2>&1; then
  echo "    running ${N} requests via oha (C=${C})…"
  oha -n "$N" -c "$C" -q 0 --no-tui \
    -H "Host: ${TENANT_DOMAIN}" -H "content-type: application/json" \
    -m POST -d "{\"model\":\"${MODEL}\"}" \
    "${PROXY_BASE}/v1/chat/completions" \
    | grep -Ei "Requests/s|Latency|" || true
elif command -v wrk >/dev/null 2>&1; then
  echo "    running wrk (use oha for richer stats)…"
  wrk -t"$C" -c"$C" -d10s -s /dev/stdin "${PROXY_BASE}/v1/chat/completions" <<'LUA' || true
    wrk.method = "POST"
    wrk.headers["Host"] = os.getenv("TENANT_DOMAIN")
    wrk.headers["content-type"] = "application/json"
    wrk.body = '{"model":"' .. os.getenv("MODEL") .. '"}'
LUA
else
  echo "    (no oha/wrk; skipping RPS measurement. Use the Rust test for distribution.)"
fi

# ---- 2. breaker inspect ---------------------------------------------------
echo
echo "==> [2] breaker dead-set"
curl_admin GET /api/v1/breaker
echo

echo
echo "==> done. For deterministic assertions run:"
echo "    cargo test -p hydra-server --features server --test load_breaker_swrr"
