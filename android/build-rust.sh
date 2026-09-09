#!/usr/bin/env bash
# Cross-compile the Quarkdrive core into the Android app's jniLibs.
#
# Requires the Android NDK and the Rust targets you want to build:
#   rustup target add aarch64-linux-android armv7-linux-androideabi \
#                   x86_64-linux-android i686-linux-android
#
# Usage: ./build-rust.sh [--release]
#   ABIS="arm64-v8a x86_64" ./build-rust.sh   # build a subset of ABIs
set -euo pipefail

PROFILE_FLAG="${1:---release}"        # cargo wants "--release" / "--dev"
PROFILE_DIR="${PROFILE_FLAG#--}"      # ...but the target dir is "release" / "debug"
: "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to your NDK}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
JNILIBS="$ROOT/android/app/src/main/jniLibs"
TOOLCHAIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64"

# Android ABI -> (rust target, clang wrapper, API level)
TARGETS=(
  "arm64-v8a aarch64-linux-android aarch64-linux-android24-clang 24"
  "armeabi-v7a armv7-linux-androideabi armv7a-linux-androideabi24-clang 24"
  "x86_64 x86_64-linux-android x86_64-linux-android24-clang 24"
  "x86 i686-linux-android i686-linux-android24-clang 24"
)

for entry in "${TARGETS[@]}"; do
  set -- $entry
  ABI="$1"; RUST_TARGET="$2"; CLANG="$3"; API="$4"

  # Skip ABIs the caller filtered out (and ones whose Rust target is missing).
  if [ -n "${ABIS:-}" ]; then
    case " $ABIS " in *" $ABI "*) ;; *) continue ;; esac
  fi
  if ! rustup target list --installed | grep -q "^$RUST_TARGET\$"; then
    echo "== $ABI skipped (rust target $RUST_TARGET not installed) =="
    continue
  fi

  echo "== $ABI ($RUST_TARGET) =="
  mkdir -p "$JNILIBS/$ABI"

  # cc-rs (build scripts of C libraries) reads CC/AR named after the triple;
  # cargo itself reads CARGO_TARGET_<TRIPLE>_LINKER for rustc. Two namespaces.
  UP_TRIPLE=$(echo "$RUST_TARGET" | tr 'a-z-' 'A-Z_')

  env "CC_${RUST_TARGET}=$TOOLCHAIN/bin/$CLANG" \
      "AR_${RUST_TARGET}=$TOOLCHAIN/bin/llvm-ar" \
      "CARGO_TARGET_${UP_TRIPLE}_LINKER=$TOOLCHAIN/bin/$CLANG" \
    cargo build --manifest-path "$ROOT/Cargo.toml" \
      --target "$RUST_TARGET" --package quarkdrive-ffi "$PROFILE_FLAG"

  cp "$ROOT/target/$RUST_TARGET/$PROFILE_DIR/libquarkdrive_ffi.so" "$JNILIBS/$ABI/"
done

echo "Shared libraries written to $JNILIBS"
