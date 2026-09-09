# Quarkdrive for Android

An Android client for a Quarkdrive vault: browse and upload files, view the
photo timeline, and back up the camera roll automatically.

It reuses the Rust core rather than reimplementing sync. Hashing and
content-defined chunking are compiled from `crates/quarkdrive-core` into
`libquarkdrive_ffi.so` (see `crates/quarkdrive-ffi`) and called over JNI, so a
photo is chunked identically on Android and on Linux.

> **Note on verification.** This module has been compiled and run: a debug
> APK was built with AGP 8.5 / Kotlin 2.0 / NDK 26, installed on an API 34
> emulator, and driven against a live server — sign-in, file listing, an
> upload (bytes verified identical server-side), and the JNI bridge (the
> core's BLAKE3 hash over a test input matches the desktop core exactly).
> Real-device use is still the final test.

## Building

1. Install the Rust Android targets and make sure `ANDROID_NDK_HOME` points at
   your NDK:

   ```sh
   rustup target add aarch64-linux-android armv7-linux-androideabi \
                     x86_64-linux-android i686-linux-android
   export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/<version>
   ```

2. Build the shared libraries and copy them into `app/src/main/jniLibs`:

   ```sh
   ./build-rust.sh --release
   ```

3. Build the app (the repo ships no Gradle wrapper; use any Gradle 8.7+ and
   a JDK 17, with `ANDROID_HOME` pointing at the SDK):

   ```sh
   gradle assembleDebug
   ```

   The APK lands in `app/build/outputs/apk/debug/`.

## What it does

- **Files** — browse the vault, upload from the device, download into the
  shared Downloads folder, delete. Uses the server's file API, so the phone
  does not need to run the Merkle sync engine.
- **Photos** — a date-grouped grid of server-generated thumbnails.
- **Backup** — a WorkManager job that runs every 15 minutes, finds photos added
  since the last run, and uploads the ones not already in the vault.

## Why backup is cheap

The worker keeps a set of content addresses it has already uploaded. Because
the address comes from the bytes rather than the filename or MediaStore id,
renaming a photo, moving it between albums, or clearing the app's data and
starting over does not cause a re-upload. The hash itself is computed by the
Rust core over JNI, so it matches what the server computes.

## Permissions

- `READ_MEDIA_IMAGES` (Android 13+) or `READ_EXTERNAL_STORAGE` (older) to find
  photos to back up.
- `INTERNET` / `ACCESS_NETWORK_STATE` for syncing.

The manifest sets `usesCleartextTraffic="true"` so you can point the app at a
plain-HTTP server on your LAN while developing. Put a real TLS terminator in
front of the server before exposing it to the internet.
