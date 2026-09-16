//! RFC 6238 TOTP two-factor authentication.
//!
//! Deliberately self-contained: base32 (RFC 4648, unpadded) and the
//! HMAC-SHA1 HOTP truncation are each a dozen lines, and shipping RFC 6238
//! test vectors as unit tests says more than a dependency would.

use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32, unpadded uppercase — the form authenticator apps show.
pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    for chunk in data.chunks(5) {
        let mut buf = [0u8; 5];
        buf[..chunk.len()].copy_from_slice(chunk);
        let bits = u64::from_be_bytes([
            0, buf[0], buf[1], buf[2], buf[3], buf[4], 0, 0,
        ]) >> 16;
        let n = chunk.len() * 8 / 5;
        for i in 0..8 {
            if i <= n {
                let idx = ((bits >> (5 * (7 - i))) & 0x1f) as usize;
                out.push(ALPHABET[idx] as char);
            }
        }
    }
    out
}

/// Decode unpadded base32; tolerant of lowercase and stray spaces, as when
/// a user retypes a secret by hand.
pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut bits: u64 = 0;
    let mut nbits = 0u32;
    let mut out = Vec::new();
    for c in text.chars() {
        if c == '=' || c == ' ' || c == '-' {
            continue;
        }
        let v = ALPHABET
            .iter()
            .position(|&a| a as char == c.to_ascii_uppercase())?;
        bits = (bits << 5) | v as u64;
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    Some(out)
}

/// The 30-second step RFC 6238 specifies.
pub const STEP: i64 = 30;

/// HOTP (RFC 4226) truncation for a time counter — a 31-bit code.
fn hotp(key: &[u8], counter: u64) -> u32 {
    let mut mac = HmacSha1::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 0x0f) as usize;
    let bin = u32::from_be_bytes([
        digest[offset],
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]) & 0x7fff_ffff;
    bin % 1_000_000 // six digits
}

/// Check a six-digit code for `now`, accepting the neighbouring step on
/// each side (clock drift between server and phone).
pub fn verify(secret_b32: &str, code: &str, now: i64) -> bool {
    let code = code.trim();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Some(key) = base32_decode(secret_b32) else {
        return false;
    };
    if key.is_empty() {
        return false;
    }
    let counter = now.div_euclid(STEP);
    let given: u32 = code.parse().unwrap_or(u32::MAX);
    for drift in [-1i64, 0, 1] {
        if hotp(&key, (counter + drift) as u64) == given {
            return true;
        }
    }
    false
}

/// A fresh secret: 20 random bytes, base32 — 160 bits, as RFC 4226 advises.
pub fn generate_secret() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut buf);
    base32_encode(&buf)
}

/// The URI authenticator apps photograph or paste.
pub fn otpauth_uri(secret_b32: &str, username: &str) -> String {
    format!(
        "otpauth://totp/Quarkdrive:{username}?secret={secret_b32}&issuer=Quarkdrive&algorithm=SHA1&digits=6&period=30"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_round_trips() {
        for case in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            b"\x00\x01\x02\xff",
        ] {
            let enc = base32_encode(case);
            assert_eq!(base32_decode(&enc).as_deref(), Some(case), "case {enc}");
        }
        // RFC 4648 §10 test vectors (padding stripped) — cross-checked
        // against Python's base64.b32encode.
        assert_eq!(base32_encode(b"f"), "MY");
        assert_eq!(base32_encode(b"fo"), "MZXQ");
        assert_eq!(base32_encode(b"foo"), "MZXW6");
        assert_eq!(base32_encode(b"foob"), "MZXW6YQ");
        assert_eq!(base32_encode(b"fooba"), "MZXW6YTB");
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn base32_tolerates_hand_typed_input() {
        assert_eq!(base32_decode("mzxw6 yt b"), base32_decode("MZXW6YTB"));
        assert_eq!(base32_decode("MZXW6YTB======"), base32_decode("MZXW6YTB"));
        assert_eq!(base32_decode("1!@#"), None);
    }

    #[test]
    fn rfc6238_test_vectors() {
        // The RFC's reference secret, "12345678901234567890" as ASCII.
        let secret = base32_encode(b"12345678901234567890");
        // RFC 6238 appendix B lists 8-digit codes; the 6-digit form is the
        // last six digits of each. When in doubt, cross-check against a
        // reference HMAC implementation.
        let cases = [
            (59i64, "287082"),
            (1_111_111_109, "081804"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ];
        for (time, code) in cases {
            assert!(verify(&secret, code, time), "{code} at t={time}");
            // The ±1 step window absorbs clock drift between server and
            // phone — and no further.
            assert!(verify(&secret, code, time - STEP), "{code} at t-1 step");
            assert!(verify(&secret, code, time + STEP), "{code} at t+1 step");
            assert!(!verify(&secret, code, time + 5 * STEP), "{code} far future");
            assert!(!verify(&secret, code, time - 5 * STEP), "{code} far past");
        }
    }

    #[test]
    fn malformed_codes_are_rejected() {
        let secret = generate_secret();
        let now = 1_700_000_000;
        assert!(!verify(&secret, "", now));
        assert!(!verify(&secret, "12345", now));
        assert!(!verify(&secret, "1234567", now));
        assert!(!verify(&secret, "abcdef", now));
        assert!(!verify("", "123456", now));
    }

    #[test]
    fn generated_secrets_decode_and_differ() {
        let a = generate_secret();
        let b = generate_secret();
        assert_ne!(a, b);
        assert!(base32_decode(&a).unwrap().len() >= 20);
    }
}
