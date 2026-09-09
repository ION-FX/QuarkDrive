#!/usr/bin/env bash
# End-to-end check: two Linux clients syncing through a real server.
#
# Builds nothing; run `cargo build` first.
#
#   ./scripts/e2e.sh            # uses target/debug
#   ./scripts/e2e.sh release    # uses target/release
#
# This suite caught a real merge bug that the unit tests missed: a deletion
# nested inside a directory that sync adopted wholesale was silently undone,
# because adopting a directory only ever added children, never pruned them.

set -uo pipefail

PROFILE="${1:-debug}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/$PROFILE"
WORK="${TMPDIR:-/tmp}/quarkdrive-e2e"
DATA="$WORK/data"
PORT="${PORT:-8899}"
SRV="http://127.0.0.1:$PORT"

if [[ ! -x "$BIN/quarkdrive-server" || ! -x "$BIN/qd" ]]; then
  echo "binaries not found in $BIN — run: cargo build${PROFILE:+ --$PROFILE}" >&2
  exit 1
fi

rm -rf "$WORK"; mkdir -p "$WORK"
FAILED=""

pass() { echo "  PASS  $1"; }
fail() { echo "  FAIL  $1"; FAILED=1; }
expect() { if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3 (expected '$2', got '$1')"; fi; }

cleanup() { [[ -n "${SRV_PID:-}" ]] && kill "$SRV_PID" 2>/dev/null; }
trap cleanup EXIT

echo "== server ($PROFILE) =="
"$BIN/quarkdrive-server" serve --data "$DATA" --listen "127.0.0.1:$PORT" >"$WORK/server.log" 2>&1 &
SRV_PID=$!

for _ in $(seq 1 60); do
  curl -sf "$SRV/api/v1/health" >/dev/null 2>&1 && break
  sleep 0.25
done
curl -sf "$SRV/api/v1/health" >/dev/null || { echo "server did not start"; tail -20 "$WORK/server.log"; exit 1; }
pass "server responds"

echo "== first-run registration =="
# The suite starts from an empty data directory, so the very first account
# is created the way a real first-run does: through the register endpoint,
# not the CLI.
expect "$(curl -s "$SRV/api/v1/auth/status" | grep -o true)" "true" "fresh server reports first-run"
REG=$(curl -s -X POST "$SRV/api/v1/auth/register" -H 'content-type: application/json' \
  -d '{"username":"ada","password":"hunter22","vault":"photos"}')
TOKEN=$(printf '%s' "$REG" | python3 -c "import json,sys; print(json.load(sys.stdin).get('token',''))")
if [[ -n "$TOKEN" ]]; then
  pass "first account + vault created via registration"
else
  fail "registration failed: $REG"
fi

auth=(-H "Authorization: Bearer $TOKEN")

expect "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$SRV/api/v1/auth/register" \
  -H 'content-type: application/json' \
  -d '{"username":"bob","password":"hunter22","vault":"other"}')" "403" "registration closes once an account exists"
expect "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" "$SRV/api/v1/whoami")" "200" "registered session authenticates"
expect "$(curl -s "$SRV/api/v1/auth/status" | grep -o true)" "" "status is no longer first-run"

echo "== device A: laptop =="
mkdir -p "$WORK/laptop"
"$BIN/qd" init --server "$SRV" --vault photos --token "$TOKEN" --device laptop --dir "$WORK/laptop" >/dev/null
echo "hello from the laptop" > "$WORK/laptop/notes.txt"
mkdir -p "$WORK/laptop/docs"
head -c 300000 /dev/urandom > "$WORK/laptop/docs/random.bin"
printf 'line\n' > "$WORK/laptop/docs/readme.md"
"$BIN/qd" sync --dir "$WORK/laptop" >/dev/null
pass "laptop synced"

echo "== server-side listing =="
curl -s "${auth[@]}" "$SRV/api/v1/vaults/photos/fs?path=" > "$WORK/list.json"
names=$(python3 -c "
import json
d = json.load(open('$WORK/list.json'))
print(','.join(sorted(e['name'] for e in d['entries'])))")
expect "$names" "docs,notes.txt" "server lists uploaded files"

echo "== device B: desktop =="
mkdir -p "$WORK/desktop"
"$BIN/qd" init --server "$SRV" --vault photos --token "$TOKEN" --device desktop --dir "$WORK/desktop" >/dev/null
"$BIN/qd" sync --dir "$WORK/desktop" >/dev/null
if diff -r --exclude=.quarkdrive "$WORK/laptop" "$WORK/desktop" >/dev/null; then
  pass "desktop received an identical copy"
else
  fail "desktop copy differs"
fi

echo "== edit on desktop, pull on laptop =="
echo "typed on the desktop" > "$WORK/desktop/from-desktop.txt"
"$BIN/qd" sync --dir "$WORK/desktop" >/dev/null
"$BIN/qd" sync --dir "$WORK/laptop" >/dev/null
expect "$(cat "$WORK/laptop/from-desktop.txt" 2>/dev/null)" "typed on the desktop" "laptop pulled the new file"

echo "== deletion propagates =="
rm "$WORK/laptop/docs/readme.md"
"$BIN/qd" sync --dir "$WORK/laptop" >/dev/null
"$BIN/qd" sync --dir "$WORK/desktop" >/dev/null
if [[ ! -e "$WORK/desktop/docs/readme.md" ]]; then
  pass "deletion reached desktop"
else
  fail "deletion did not propagate"
fi

echo "== idempotency =="
out=$("$BIN/qd" sync --dir "$WORK/laptop")
expect "$out" "up to date" "idle sync reports nothing to do"

echo "== concurrent edits both survive =="
echo "original" > "$WORK/laptop/shared.txt"
"$BIN/qd" sync --dir "$WORK/laptop" >/dev/null
"$BIN/qd" sync --dir "$WORK/desktop" >/dev/null
# Both edit without seeing the other; the desktop's edit is newer.
echo "laptop version" > "$WORK/laptop/shared.txt"
sleep 1.1
echo "desktop version" > "$WORK/desktop/shared.txt"
touch -d "+10 seconds" "$WORK/desktop/shared.txt"
"$BIN/qd" sync --dir "$WORK/desktop" >/dev/null
"$BIN/qd" sync --dir "$WORK/laptop" >/dev/null
expect "$(cat "$WORK/laptop/shared.txt")" "desktop version" "newer edit wins on the laptop"
if ls "$WORK/laptop/shared.txt.conflict-laptop-"* >/dev/null 2>&1; then
  pass "losing edit preserved as a conflict copy"
else
  fail "no conflict copy was written"
fi

echo "== web API interop =="
curl -s -X PUT "${auth[@]}" --data-binary 'uploaded via the web API' \
     "$SRV/api/v1/vaults/photos/fs?path=web/hello.txt" >/dev/null
"$BIN/qd" sync --dir "$WORK/laptop" >/dev/null
expect "$(cat "$WORK/laptop/web/hello.txt" 2>/dev/null)" "uploaded via the web API" "client pulled a web upload"

echo "== auth is enforced =="
expect "$(curl -s -o /dev/null -w '%{http_code}' "$SRV/api/v1/vaults/photos/fs?path=")" "401" "unauthenticated request rejected"
expect "$(curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer bogus' "$SRV/api/v1/vaults/photos/fs?path=")" "401" "bad token rejected"

echo "== unknown API endpoints fail honestly =="
# A missing route once fell through to the static UI and answered index.html
# with 200, which hid real client bugs (a mistyped endpoint looked like an
# empty successful response).
expect "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" "$SRV/api/v1/vaults/photos/bogus")" "404" "unknown API path returns 404"
expect "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" "$SRV/api/typo")" "404" "typo'd API root returns 404"
body=$(curl -s "${auth[@]}" "$SRV/api/v1/vaults/photos/bogus")
case "$body" in
  *"<html"*|*"<!DOCTYPE"*) fail "API 404 body is HTML";;
  *"no such API endpoint"*) pass "API 404 body is a JSON error";;
  *) fail "API 404 body unexpected: $body";;
esac

echo "== hostile paths are rejected =="
for p in "../escape" "%2e%2e%2fescape" "bad%0Aname" "bad%00name"; do
  code=$(curl -s -o /dev/null -w '%{http_code}' -X PUT "${auth[@]}" \
         --data-binary x "$SRV/api/v1/vaults/photos/fs?path=$p")
  expect "$code" "400" "rejects path '$p'"
done

echo "== encrypted vault: server stores only ciphertext =="
# The file API refuses encrypted vaults by design; only `qd` clients holding
# the key file can use one. This exercises the seam the unit tests cover only
# from each side: a real init --encrypt, sync up, sync down on a second client.
"$BIN/quarkdrive-server" create-vault --data "$DATA" --username ada --name safe --encrypted >/dev/null
mkdir -p "$WORK/enc-a"
"$BIN/qd" init --server "$SRV" --vault safe --token "$TOKEN" --device enc-a --encrypt --dir "$WORK/enc-a" >/dev/null
echo "classified material" > "$WORK/enc-a/plans.txt"
mkdir -p "$WORK/enc-a/private"
head -c 200000 /dev/urandom > "$WORK/enc-a/private/data.bin"
"$BIN/qd" sync --dir "$WORK/enc-a" >/dev/null
pass "encrypted client synced"
if grep -rq "classified material" "$DATA" 2>/dev/null; then
  fail "plaintext found in the server's data directory"
else
  pass "plaintext never reaches the server"
fi
mkdir -p "$WORK/enc-b"
"$BIN/qd" init --server "$SRV" --vault safe --token "$TOKEN" --device enc-b --encrypt --dir "$WORK/enc-b" >/dev/null
cp "$WORK/enc-a/.quarkdrive/key" "$WORK/enc-b/.quarkdrive/key"
"$BIN/qd" sync --dir "$WORK/enc-b" >/dev/null
expect "$(cat "$WORK/enc-b/plans.txt" 2>/dev/null)" "classified material" "second client decrypted the vault"
if diff -r --exclude=.quarkdrive "$WORK/enc-a" "$WORK/enc-b" >/dev/null; then
  pass "decrypted copy is byte-identical"
else
  fail "decrypted copy differs"
fi
# And the file API must still refuse the encrypted vault.
expect "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" "$SRV/api/v1/vaults/safe/fs?path=")" \
  "400" "file API refuses an encrypted vault"

echo
if [[ -z "$FAILED" ]]; then
  echo "ALL END-TO-END CHECKS PASSED"
  exit 0
else
  echo "SOME CHECKS FAILED"
  exit 1
fi
