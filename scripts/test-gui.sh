#!/usr/bin/env bash
# End-to-end test of the native GUI against a real server, headlessly.
#
# Starts a throwaway quarkdrive-server on a scratch data directory,
# registers a test account, then runs the GUI's integration test, which
# drives the real app through egui frames and writes screenshots to
# /tmp/qd-gui-shots (override with QD_SHOTS).
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${QD_TEST_PORT:-8931}"
DATA="$(mktemp -d /tmp/qd-gui-server.XXXXXX)"
DOWNLOADS="$(mktemp -d /tmp/qd-gui-downloads.XXXXXX)"
export QD_SHOTS="${QD_SHOTS:-/tmp/qd-gui-shots}"
USER_NAME="gui-test"
USER_PASS="gui-test-pass"

SERVER_PID=""
cleanup() {
    if [[ -n "$SERVER_PID" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$DATA"
}
trap cleanup EXIT

echo "== building server + gui =="
cargo build --release -p quarkdrive-server -p quarkdrive-gui

echo "== starting throwaway server on 127.0.0.1:$PORT =="
./target/release/quarkdrive-server serve \
    --data "$DATA" --web ./web --listen "127.0.0.1:$PORT" \
    >"$DATA/server.log" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 50); do
    if curl -fsS "http://127.0.0.1:$PORT/api/v1/health" >/dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
curl -fsS "http://127.0.0.1:$PORT/api/v1/health" >/dev/null

echo "== registering $USER_NAME =="
curl -fsS -X POST "http://127.0.0.1:$PORT/api/v1/auth/register" \
    -H 'Content-Type: application/json' \
    -d "{\"username\":\"$USER_NAME\",\"password\":\"$USER_PASS\",\"vault\":\"$USER_NAME\"}" >/dev/null

echo "== running headless GUI tests =="
export QD_TEST_SERVER="http://127.0.0.1:$PORT"
export QD_TEST_USER="$USER_NAME"
export QD_TEST_PASS="$USER_PASS"
export QD_DOWNLOAD_DIR="$DOWNLOADS"
export QD_TEST_MODE=1
cargo test --release -p quarkdrive-gui --test headless -- --nocapture

echo
echo "== server requests seen during the run =="
grep -oE 'GET /api/v1[^ ]*|POST /api/v1[^ ]*|PUT /api/v1[^ ]*|DELETE /api/v1[^ ]*' "$DATA/server.log" | sort | uniq -c | sort -rn | head -20

echo "PASS: GUI end-to-end (screenshots in $QD_SHOTS)"
