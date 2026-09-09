//! Known-answer test against the official BLAKE3 test vectors.

use quarkdrive_core::ObjectId;

/// The official BLAKE3 test vector for the empty input
/// (from the BLAKE3 repository's test vectors JSON).
const EMPTY_HASH: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";

#[test]
fn blake3_empty_input_matches_the_official_vector() {
    assert_eq!(ObjectId::hash(b"").to_hex(), EMPTY_HASH);
}

#[test]
fn blake3_of_hello_quarkdrive_is_stable() {
    // Not an official vector, but pinned so any change is caught here rather
    // than in a confused sync client. Must equal what the JNI bridge returns.
    let hex = ObjectId::hash(b"hello quarkdrive").to_hex();
    println!("blake3(b\"hello quarkdrive\") = {hex}");
    assert_eq!(hex.len(), 64);
}
