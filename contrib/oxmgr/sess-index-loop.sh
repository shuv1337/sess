#!/usr/bin/env bash
# Periodic sess incremental index for oxmgr (or any long-running supervisor).
#
# systemd equivalent: contrib/systemd/sess-index.{service,timer}
# Default interval matches the 15m max-age freshness threshold.

set -uo pipefail

INTERVAL_SECS="${SESS_INDEX_INTERVAL_SECS:-900}"
SESS_BIN="${SESS_BIN:-$(command -v sess || true)}"
# Prefer the installed shim; fall back to the release build in this repo.
if [[ -z "${SESS_BIN}" ]]; then
  SESS_BIN="$(cd "$(dirname "$0")/../.." && pwd)/target/release/sess"
fi

# Semantic embeddings ON by default so new/updated conversations get vectors.
# Set SESS_INDEX_NO_SEMANTIC=1 to match the systemd unit's conservative default.
EXTRA_FLAGS=()
if [[ "${SESS_INDEX_NO_SEMANTIC:-0}" == "1" ]]; then
  EXTRA_FLAGS+=(--no-semantic)
fi

echo "[$(date -Is)] sess-index-loop start (interval=${INTERVAL_SECS}s, bin=${SESS_BIN}, flags=${EXTRA_FLAGS[*]:-<none>})"

while true; do
  echo "[$(date -Is)] sess index starting"
  if "${SESS_BIN}" "${EXTRA_FLAGS[@]}" index; then
    echo "[$(date -Is)] sess index ok"
  else
    rc=$?
    echo "[$(date -Is)] sess index failed (exit ${rc})" >&2
  fi
  sleep "${INTERVAL_SECS}"
done
