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

echo "== vault sharing =="
"$BIN/quarkdrive-server" create-user  --data "$DATA" --username bob   --password 'builder-7' >/dev/null
"$BIN/quarkdrive-server" create-user  --data "$DATA" --username carol --password 'singer-8'  >/dev/null
BOB_TOKEN=$("$BIN/quarkdrive-server" create-token --data "$DATA" --username bob | tail -1)
CAROL_TOKEN=$("$BIN/quarkdrive-server" create-token --data "$DATA" --username carol | tail -1)
bob_auth=(-H "Authorization: Bearer $BOB_TOKEN")
carol_auth=(-H "Authorization: Bearer $CAROL_TOKEN")

curl -sf -X POST "${auth[@]}" "$SRV/api/v1/vaults/photos/shares" \
  -H 'content-type: application/json' -d '{"username":"bob","role":"write"}' >/dev/null
pass "owner shared the vault with bob (write)"
curl -sf -X POST "${auth[@]}" "$SRV/api/v1/vaults/photos/shares" \
  -H 'content-type: application/json' -d '{"username":"carol","role":"read"}' >/dev/null
pass "owner shared the vault with carol (read)"

role=$(curl -s "${bob_auth[@]}" "$SRV/api/v1/vaults" | python3 -c "
import json,sys
vs = json.load(sys.stdin)['vaults']
print(next(v['role'] for v in vs if v['name'] == 'photos'))")
expect "$role" "write" "shared vault appears in bob's list with the right role"

expect "$(curl -s -o /dev/null -w '%{http_code}' "${bob_auth[@]}" "$SRV/api/v1/vaults/photos/fs?path=")" \
  "200" "write-share can list"
expect "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "${bob_auth[@]}" --data-binary 'from bob' \
         "$SRV/api/v1/vaults/photos/fs?path=bob.txt")" \
  "200" "write-share can upload"
expect "$(curl -s -o /dev/null -w '%{http_code}' "${carol_auth[@]}" "$SRV/api/v1/vaults/photos/fs?path=")" \
  "200" "read-share can list"
expect "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "${carol_auth[@]}" --data-binary 'nope' \
         "$SRV/api/v1/vaults/photos/fs?path=carol.txt")" \
  "403" "read-share cannot upload"
expect "$(curl -s -o /dev/null -w '%{http_code}' "${bob_auth[@]}" "$SRV/api/v1/vaults/photos/shares")" \
  "403" "sharee cannot manage shares"
expect "$(curl -s -o /dev/null -w '%{http_code}' -X POST "${auth[@]}" "$SRV/api/v1/vaults/photos/shares" \
         -H 'content-type: application/json' -d '{"username":"ghost","role":"read"}')" \
  "404" "sharing with an unknown user fails"
expect "$(curl -s -o /dev/null -w '%{http_code}' -X POST "${auth[@]}" "$SRV/api/v1/vaults/safe/shares" \
         -H 'content-type: application/json' -d '{"username":"bob","role":"read"}')" \
  "400" "encrypted vaults cannot be shared"

curl -sf -X DELETE "${auth[@]}" "$SRV/api/v1/vaults/photos/shares/bob" >/dev/null
expect "$(curl -s -o /dev/null -w '%{http_code}' "${bob_auth[@]}" "$SRV/api/v1/vaults/photos/fs?path=")" \
  "404" "revoked share loses access, indistinguishable from no vault"

echo "== trash: delete, restore, purge =="
curl -sf -X PUT "${auth[@]}" --data-binary 'the quarterly report' \
     "$SRV/api/v1/vaults/photos/fs?path=report.txt" >/dev/null
curl -sf -X DELETE "${auth[@]}" "$SRV/api/v1/vaults/photos/fs?path=report.txt" >/dev/null
expect "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" "$SRV/api/v1/vaults/photos/fs/download?path=report.txt")" \
  "404" "deleted file is gone from the vault"
TRASH_ID=$(curl -s "${auth[@]}" "$SRV/api/v1/vaults/photos/trash" | python3 -c "
import json,sys
items = json.load(sys.stdin)['items']
print(items[0]['id'] if items else '')")
if [[ -n "$TRASH_ID" ]]; then
  pass "deleted file is listed in the trash"
else
  fail "trash is empty after a delete"
fi
RESTORED=$(curl -s -X POST "${auth[@]}" "$SRV/api/v1/vaults/photos/trash/restore?id=$TRASH_ID")
expect "$(printf '%s' "$RESTORED" | python3 -c "import json,sys; print(json.load(sys.stdin).get('path',''))")" \
  "report.txt" "restore puts the file back at its old path"
expect "$(curl -s "${auth[@]}" "$SRV/api/v1/vaults/photos/fs/download?path=report.txt")" \
  "the quarterly report" "restored bytes are identical"

# Deleting again while the path is occupied restores under a new name.
curl -sf -X DELETE "${auth[@]}" "$SRV/api/v1/vaults/photos/fs?path=report.txt" >/dev/null
curl -sf -X PUT "${auth[@]}" --data-binary 'replaced' \
     "$SRV/api/v1/vaults/photos/fs?path=report.txt" >/dev/null
TRASH_ID2=$(curl -s "${auth[@]}" "$SRV/api/v1/vaults/photos/trash" | python3 -c "
import json,sys; print(json.load(sys.stdin)['items'][0]['id'])")
RESTORED2=$(curl -s -X POST "${auth[@]}" "$SRV/api/v1/vaults/photos/trash/restore?id=$TRASH_ID2")
case "$RESTORED2" in
  *".restored-"*) pass "restore renames around an occupied path";;
  *) fail "restore into an occupied path: $RESTORED2";;
esac

expect "$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "${carol_auth[@]}" "$SRV/api/v1/vaults/photos/trash?all=true")" \
  "403" "read-only share cannot purge the trash"
curl -sf -X DELETE "${auth[@]}" "$SRV/api/v1/vaults/photos/trash?all=true" >/dev/null
count=$(curl -s "${auth[@]}" "$SRV/api/v1/vaults/photos/trash" | python3 -c "import json,sys; print(len(json.load(sys.stdin)['items']))")
expect "$count" "0" "purge empties the trash"

echo "== file versions =="
curl -sf -X PUT "${auth[@]}" --data-binary 'first draft' \
     "$SRV/api/v1/vaults/photos/fs?path=versions.txt" >/dev/null
curl -sf -X PUT "${auth[@]}" --data-binary 'second draft, much longer than the first' \
     "$SRV/api/v1/vaults/photos/fs?path=versions.txt" >/dev/null
VCOUNT=$(curl -s "${auth[@]}" "$SRV/api/v1/vaults/photos/fs/versions?path=versions.txt" | python3 -c "
import json,sys; print(len(json.load(sys.stdin)['items']))")
expect "$VCOUNT" "2" "two contents of the same path are two versions"
OLD_ID=$(curl -s "${auth[@]}" "$SRV/api/v1/vaults/photos/fs/versions?path=versions.txt" | python3 -c "
import json,sys; print(json.load(sys.stdin)['items'][-1]['id'])")
expect "$(curl -s "${auth[@]}" "$SRV/api/v1/vaults/photos/fs/versions/download?path=versions.txt&id=$OLD_ID")" \
  "first draft" "an old version's bytes come back intact"
curl -sf -X POST "${auth[@]}" \
     "$SRV/api/v1/vaults/photos/fs/versions/restore?path=versions.txt&id=$OLD_ID" >/dev/null
expect "$(curl -s "${auth[@]}" "$SRV/api/v1/vaults/photos/fs/download?path=versions.txt")" \
  "first draft" "restoring an old version replaces the current file"
expect "$(curl -s -o /dev/null -w '%{http_code}' "${carol_auth[@]}" "$SRV/api/v1/vaults/photos/fs/versions/restore?path=versions.txt&id=$OLD_ID" -X POST)" \
  "403" "read-only share cannot restore versions"

echo "== public links =="
curl -sf -X PUT "${auth[@]}" --data-binary 'readable by the world' \
     "$SRV/api/v1/vaults/photos/fs?path=shared/public.txt" >/dev/null
LINK_ID=$(curl -s -X POST "${auth[@]}" "$SRV/api/v1/vaults/photos/links" \
  -H 'content-type: application/json' -d '{"path":"shared"}' | python3 -c "
import json,sys; print(json.load(sys.stdin).get('id',''))")
if [[ -n "$LINK_ID" ]]; then
  pass "link created"
else
  fail "link creation failed"
fi
expect "$(curl -s -o /dev/null -w '%{http_code}' "$SRV/api/v1/public/$LINK_ID")" \
  "200" "the link answers without any account"
expect "$(curl -s "$SRV/api/v1/public/$LINK_ID/list?path=" | python3 -c "
import json,sys; print(json.load(sys.stdin)['items'][0]['name'])")" \
  "public.txt" "the link lists the shared folder"
expect "$(curl -s "$SRV/api/v1/public/$LINK_ID/download?path=public.txt")" \
  "readable by the world" "the link downloads the file"
# Visitors cannot leave the shared folder.
for esc in "..%2Fnotes.txt" "%2e%2e%2Fvaults"; do
  expect "$(curl -s -o /dev/null -w '%{http_code}' "$SRV/api/v1/public/$LINK_ID/download?path=$esc")" \
    "400" "link path '$esc' is refused"
done
# Password-protected link.
PLINK_ID=$(curl -s -X POST "${auth[@]}" "$SRV/api/v1/vaults/photos/links" \
  -H 'content-type: application/json' -d '{"path":"shared","password":"hush now"}' | python3 -c "
import json,sys; print(json.load(sys.stdin).get('id',''))")
expect "$(curl -s -o /dev/null -w '%{http_code}' "$SRV/api/v1/public/$PLINK_ID")" \
  "401" "a password link demands the password"
expect "$(curl -s -o /dev/null -w '%{http_code}' "$SRV/api/v1/public/$PLINK_ID" -H 'X-Link-Password: wrong')" \
  "401" "a wrong password is refused"
expect "$(curl -s -o /dev/null -w '%{http_code}' "$SRV/api/v1/public/$PLINK_ID" -H 'X-Link-Password: hush now')" \
  "200" "the right password opens the link"
# Expiry.
ELINK_ID=$(curl -s -X POST "${auth[@]}" "$SRV/api/v1/vaults/photos/links" \
  -H 'content-type: application/json' -d '{"path":"shared","expires_secs":1}' | python3 -c "
import json,sys; print(json.load(sys.stdin).get('id',''))")
sleep 1.5
expect "$(curl -s -o /dev/null -w '%{http_code}' "$SRV/api/v1/public/$ELINK_ID")" \
  "410" "an expired link says so"
# Revocation.
curl -sf -X DELETE "${auth[@]}" "$SRV/api/v1/vaults/photos/links/$LINK_ID" >/dev/null
expect "$(curl -s -o /dev/null -w '%{http_code}' "$SRV/api/v1/public/$LINK_ID")" \
  "404" "a removed link stops answering"
# The visitor page itself.
case "$(curl -s "$SRV/s/$PLINK_ID")" in
  *public.js*) pass "the /s/<id> page serves the visitor UI";;
  *) fail "/s/<id> did not serve the visitor page";;
esac

echo "== login rate limiting =="
for i in 1 2 3 4 5 6; do
  code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$SRV/api/v1/auth/login" \
    -H 'content-type: application/json' -d '{"username":"carol","password":"wrong"}')
  case "$i" in
    1|2|3|4|5) expect "$code" "401" "bad login $i rejected";;
    6)         expect "$code" "429" "sixth attempt is rate-limited";;
  esac
done
expect "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$SRV/api/v1/auth/login" \
  -H 'content-type: application/json' -d '{"username":"carol","password":"singer-8"}')" \
  "429" "the correct password is also refused while locked out"

echo "== HTTPS (TLS) =="
# A second server process on the same data, behind a self-signed cert.
TLS_PORT="${TLS_PORT:-8898}"
openssl req -x509 -newkey rsa:2048 -keyout "$WORK/key.pem" -out "$WORK/cert.pem" \
  -days 2 -nodes -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost" 2>/dev/null
"$BIN/quarkdrive-server" serve --data "$DATA" --listen "127.0.0.1:$TLS_PORT" \
  --tls-cert "$WORK/cert.pem" --tls-key "$WORK/key.pem" >"$WORK/tls.log" 2>&1 &
TLS_PID=$!
for _ in $(seq 1 60); do
  curl -sfk "https://127.0.0.1:$TLS_PORT/api/v1/health" >/dev/null 2>&1 && break
  sleep 0.25
done
expect "$(curl -sfk "https://127.0.0.1:$TLS_PORT/api/v1/health" | grep -o '"ok":true')" \
  '"ok":true' "server serves HTTPS"
subject=$(echo | openssl s_client -connect "127.0.0.1:$TLS_PORT" 2>/dev/null | \
  openssl x509 -noout -subject 2>/dev/null)
case "$subject" in
  *CN*localhost*) pass "TLS handshake presents the certificate";;
  *) fail "could not verify TLS handshake: $subject";;
esac
kill "$TLS_PID" 2>/dev/null

echo
if [[ -z "$FAILED" ]]; then
  echo "ALL END-TO-END CHECKS PASSED"
  exit 0
else
  echo "SOME CHECKS FAILED"
  exit 1
fi
