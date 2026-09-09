#!/usr/bin/env bash
# Set up a Quarkdrive server: build it, create a user and a vault, and print
# what you need to sign in.
#
#   ./scripts/setup.sh                          # ask questions, build, set up
#   ./scripts/setup.sh --start                  # ... and start the server
#   ./scripts/setup.sh --user ada --password s3cret \
#       --vault photos --port 8787 --start      # no questions asked
#
# Safe to re-run. Existing data is never overwritten; an existing username is
# left alone (its password is not changed).

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"

PROFILE="release"
DATA_DIR="$ROOT/quarkdrive-data"
WEB_DIR="$ROOT/web"
HOST="0.0.0.0"
PORT="8787"
USERNAME="" PASSWORD="" VAULT="" START=0 ASSUME_YES=0

die() { echo "error: $*" >&2; exit 1; }
info() { echo "  $*"; }
hr() { echo "---------------------------------------------------------------"; }

usage() {
  sed -n '2,14p' "$0"
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --data) DATA_DIR="$2"; shift 2 ;;
    --web) WEB_DIR="$2"; shift 2 ;;
    --user) USERNAME="$2"; shift 2 ;;
    --password) PASSWORD="$2"; shift 2 ;;
    --vault) VAULT="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    --host) HOST="$2"; shift 2 ;;
    --profile) PROFILE="$2"; shift 2 ;;
    --start) START=1; shift ;;
    --yes|-y) ASSUME_YES=1; shift ;;
    -h|--help) usage ;;
    *) die "unknown option: $1 (try --help)" ;;
  esac
done

[[ "$PROFILE" == "release" || "$PROFILE" == "debug" ]] || die "--profile must be release or debug"
[[ "$PORT" =~ ^[0-9]+$ ]] || die "--port must be a number"

# ----------------------------------------------------------------- build

BIN="$ROOT/target/$PROFILE"
SERVER="$BIN/quarkdrive-server"
CLIENT="$BIN/qd"

if [[ -x "$SERVER" && -x "$CLIENT" ]]; then
  info "already built: $SERVER"
else
  info "building (this takes a few minutes the first time)…"
  command -v cargo >/dev/null 2>&1 || die "cargo not found — install Rust from https://rustup.rs"
  cargo build ${PROFILE:+--$PROFILE}
fi

# ----------------------------------------------------------------- prompts

ask() { # ask <variable name> <prompt> <default>
  local __var="$1" __prompt="$2" __default="$3" answer
  if [[ -n "${!__var}" ]]; then return; fi
  if [[ "$ASSUME_YES" == 1 ]]; then
    printf -v "$__var" '%s' "$__default"
    return
  fi
  read -r -p "$__prompt [$__default]: " answer
  printf -v "$__var" '%s' "${answer:-$__default}"
}

echo
hr
echo "Quarkdrive setup"
hr
echo

ask USERNAME "Administrator username" "demo"
ask VAULT "Name of the first vault" "photos"
ask PORT "Port to listen on" "$PORT"
ask HOST "Address to bind" "$HOST"

if [[ -z "$PASSWORD" ]]; then
  if [[ "$ASSUME_YES" == 1 ]]; then
    die "--password is required with --yes"
  fi
  while true; do
    read -rs -p "Password for $USERNAME: " PASSWORD; echo
    [[ -n "$PASSWORD" ]] || { echo "  password must not be empty"; continue; }
    read -rs -p "Confirm password: " REPEAT; echo
    [[ "$PASSWORD" == "$REPEAT" ]] || { echo "  passwords do not match"; PASSWORD=""; continue; }
    break
  done
fi

mkdir -p "$DATA_DIR"

echo
info "creating user '$USERNAME'"
if output=$("$SERVER" create-user --data "$DATA_DIR" --username "$USERNAME" --password "$PASSWORD" 2>&1); then
  info "  $(echo "$output" | head -1)"
else
  info "  skipped — $(echo "$output" | head -1 | sed 's/^error: //')"
  info "  (the existing password is unchanged)"
fi

info "creating vault '$VAULT'"
if output=$("$SERVER" create-vault --data "$DATA_DIR" --username "$USERNAME" --name "$VAULT" 2>&1); then
  info "  $(echo "$output" | head -1)"
else
  info "  skipped — $(echo "$output" | head -1 | sed 's/^error: //')"
fi

info "issuing an access token"
TOKEN=$("$SERVER" create-token --data "$DATA_DIR" --username "$USERNAME" --device setup)
TOKEN_FILE="$DATA_DIR/$USERNAME.token"
printf '%s\n' "$TOKEN" > "$TOKEN_FILE"
chmod 600 "$TOKEN_FILE"
info "  saved to $TOKEN_FILE (mode 600)"

# ----------------------------------------------------------------- summary

HR="---------------------------------------------------------------"

# The address a browser on another machine would use, if we can tell.
LAN_IP="$(hostname -I 2>/dev/null | awk '{print $1}' | tr -d ' ')"
PUBLIC_ADDR="http://<this-machine>:$PORT"
if [[ -n "$LAN_IP" ]]; then
  PUBLIC_ADDR="http://$LAN_IP:$PORT"
fi
cat <<SUMMARY

$HR
Setup complete.

  Server     : http://$HOST:$PORT
  Web UI     : $PUBLIC_ADDR
  Username   : $USERNAME
  Vault      : $VAULT
  Data       : $DATA_DIR
  Token file : $TOKEN_FILE

Sign in at the address above with $USERNAME and the password you chose.

To sync a folder from this machine:

  $CLIENT init --server http://localhost:$PORT --vault $VAULT \\
               --token "\$(cat $TOKEN_FILE)" --device \$(hostname) --dir ~/Quarkdrive
  $CLIENT watch --dir ~/Quarkdrive

To start the server again later:

  $SERVER serve --data $DATA_DIR --web $WEB_DIR --listen $HOST:$PORT
SUMMARY

# ----------------------------------------------------------------- start

if [[ "$START" == 1 ]]; then
  echo
  info "starting the server…"
  if ! command -v setsid >/dev/null 2>&1; then
    die "setsid is required for --start"
  fi
  setsid nohup "$SERVER" serve \
    --data "$DATA_DIR" --web "$WEB_DIR" --listen "$HOST:$PORT" \
    >> "$DATA_DIR/server.log" 2>&1 < /dev/null &

  for _ in $(seq 1 60); do
    curl -sf "http://127.0.0.1:$PORT/api/v1/health" >/dev/null 2>&1 && break
    sleep 0.25
  done

  if curl -sf "http://127.0.0.1:$PORT/api/v1/health" >/dev/null 2>&1; then
    info "running — open $PUBLIC_ADDR"
    info "log: $DATA_DIR/server.log"
  else
    die "did not come up — see $DATA_DIR/server.log"
  fi
fi
