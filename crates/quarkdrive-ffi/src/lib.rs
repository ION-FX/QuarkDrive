//! JNI bindings for Android.
//!
//! Android runs the same engine as Linux rather than a second implementation:
//! chunking and hashing are compiled from `quarkdrive-core` into
//! `libquarkdrive_ffi.so`, which the Kotlin client loads with
//! `System.loadLibrary("quarkdrive_ffi")`.
//!
//! The surface here is deliberately narrow and side-effect free — bytes in,
//! identifiers or offsets out. Everything stateful (networking, the index of
//! what has been backed up, upload scheduling) stays in Kotlin, close to the
//! platform APIs it needs.
//!
//! Names follow the JNI convention `Java_<package>_<Class>_<method>`, matching
//! `dev.quarkdrive.android.QuarkdriveNative`.

use jni::objects::{JByteArray, JClass};
use jni::sys::{jint, jintArray, jsize, jstring};
use jni::JNIEnv;

use quarkdrive_core::chunker;
use quarkdrive_core::hash;

/// Content address of a buffer, as hex.
///
/// The client hashes each photo before uploading. Because the vault is
/// content-addressed, a photo that is already in the vault — from any device,
/// under any name — needs no upload at all.
#[no_mangle]
pub extern "system" fn Java_dev_quarkdrive_android_QuarkdriveNative_nativeContentHash<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    data: JByteArray<'local>,
) -> jstring {
    let bytes = match env.convert_byte_array(&data) {
        Ok(b) => b,
        Err(_) => return std::ptr::null_mut(),
    };
    jstring_of(&mut env, &hash::ObjectId::hash(&bytes).to_hex())
}

/// Lengths of the content-defined chunks a buffer splits into.
///
/// Lets the client upload a large video as chunks instead of one blob, so an
/// interrupted upload can resume and unchanged regions are skipped.
#[no_mangle]
pub extern "system" fn Java_dev_quarkdrive_android_QuarkdriveNative_nativeChunkLengths<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    data: JByteArray<'local>,
) -> jintArray {
    let bytes = match env.convert_byte_array(&data) {
        Ok(b) => b,
        Err(_) => return std::ptr::null_mut(),
    };
    let lengths: Vec<jint> = chunker::chunk_slices(&bytes)
        .iter()
        .map(|c| c.len() as jint)
        .collect();
    int_array_of(&mut env, &lengths)
}

/// `[min, avg, max]` chunk sizes, so the client can size its buffers.
#[no_mangle]
pub extern "system" fn Java_dev_quarkdrive_android_QuarkdriveNative_nativeChunkLimits<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jintArray {
    let limits: [jint; 3] = [
        chunker::MIN_CHUNK as jint,
        chunker::AVG_CHUNK as jint,
        chunker::MAX_CHUNK as jint,
    ];
    int_array_of(&mut env, &limits)
}

/// Library version, for the app's about screen and bug reports.
#[no_mangle]
pub extern "system" fn Java_dev_quarkdrive_android_QuarkdriveNative_nativeVersion<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    jstring_of(&mut env, env!("CARGO_PKG_VERSION"))
}

/// Build a Java String, or null if the JVM refuses (out of memory, say).
fn jstring_of(env: &mut JNIEnv, s: &str) -> jstring {
    match env.new_string(s) {
        Ok(java_string) => java_string.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn int_array_of(env: &mut JNIEnv, values: &[jint]) -> jintArray {
    let array = match env.new_int_array(values.len() as jsize) {
        Ok(a) => a,
        Err(_) => return std::ptr::null_mut(),
    };
    if env.set_int_array_region(&array, 0, values).is_err() {
        return std::ptr::null_mut();
    }
    array.into_raw()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The logic behind the JNI wrappers, exercised without a JVM.
    #[test]
    fn content_hash_is_stable_and_hex() {
        let hex = hash::ObjectId::hash(b"a photo").to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, hash::ObjectId::hash(b"a photo").to_hex());
        assert_ne!(hex, hash::ObjectId::hash(b"a photp").to_hex());
    }

    #[test]
    fn chunk_lengths_sum_to_the_input_length() {
        let data = vec![7u8; 500_000];
        let chunks = chunker::chunk_slices(&data);
        let total: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(total, data.len());
        assert!(!chunks.is_empty());
    }

    #[test]
    fn chunk_limits_are_ordered() {
        assert!(chunker::MIN_CHUNK < chunker::AVG_CHUNK);
        assert!(chunker::AVG_CHUNK < chunker::MAX_CHUNK);
    }
}
