//! Port of gateway/platforms/qqbot/crypto.py.
//!
//! AES-256-GCM utilities for QQBot scan-to-configure credential decryption.
//!
//! Two entry points, matching the Python module one for one:
//!
//! * [`generate_bind_key`] mints a fresh 256-bit AES key and returns it as
//!   standard base64. The CLI keeps this key locally and hands it to the bind
//!   task so the server can encrypt the bot's `client_secret` before returning
//!   it. Only this process holds the key, so the secret never travels in
//!   plaintext.
//! * [`decrypt_secret`] takes the `bot_encrypt_secret` value from the poll
//!   result plus that base64 key and returns the decrypted `client_secret`.
//!   The ciphertext layout after base64-decoding is:
//!
//!   ```text
//!   IV (12 bytes) || ciphertext (N bytes) || AuthTag (16 bytes)
//!   ```
//!
//!   The Python code hands `ciphertext || tag` (everything after the 12-byte
//!   IV) straight to `AESGCM.decrypt`, which expects the tag appended to the
//!   ciphertext. There is no additional authenticated data (`aad = None`).
//!
//! The AEAD open (AES-256-GCM) uses the RustCrypto `aes-gcm` crate rather than a
//! hand-rolled implementation. Everything else is implemented here: base64
//! encode/decode, kernel-CSPRNG key generation, and the IV / ciphertext+tag byte
//! split.
// Public API is ahead of its callers (the QQBot adapter is not ported yet).
#![allow(dead_code)]

use std::fmt;

/// Number of leading bytes that hold the GCM nonce (IV).
const IV_LEN: usize = 12;

/// AES key length in bytes (256-bit).
const KEY_LEN: usize = 32;

/// Errors from the QQBot crypto helpers. Faithful to the Python failure modes
/// (bad base64, decrypt/auth failure, non-UTF-8 plaintext) plus one the Rust
/// port adds on purpose: a CSPRNG that could not be read (fail closed rather
/// than emit a guessable key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoError {
    /// The kernel CSPRNG could not be read, so no key was minted.
    RandomUnavailable,
    /// A base64 input was not valid standard base64.
    InvalidBase64,
    /// AES-GCM authentication/decryption failed (mirrors `InvalidTag`).
    Decrypt,
    /// The decrypted bytes were not valid UTF-8.
    InvalidUtf8,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CryptoError::RandomUnavailable => write!(f, "kernel CSPRNG unavailable"),
            CryptoError::InvalidBase64 => write!(f, "invalid base64 input"),
            CryptoError::Decrypt => write!(f, "AES-GCM decryption failed"),
            CryptoError::InvalidUtf8 => write!(f, "decrypted secret was not valid UTF-8"),
        }
    }
}

impl std::error::Error for CryptoError {}

// ----- kernel CSPRNG -----

/// Fill `buf` from the kernel CSPRNG. The bind key is the only thing standing
/// between the wire and the bot's `client_secret`, so it MUST come from a
/// cryptographically secure source (Python uses `os.urandom`), never a time or
/// pid-seeded PRNG. Reads `/dev/urandom`, falling back to the `getrandom(2)`
/// syscall on Linux. Returns `false` when no CSPRNG could be read, so callers
/// fail closed (mint nothing) rather than emit a guessable value.
fn fill_random(buf: &mut [u8]) -> bool {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return true;
        }
    }
    #[cfg(target_os = "linux")]
    {
        let mut filled = 0usize;
        while filled < buf.len() {
            // SAFETY: writing into our own buffer for the remaining length.
            let rc = unsafe {
                libc::getrandom(
                    buf[filled..].as_mut_ptr() as *mut libc::c_void,
                    buf.len() - filled,
                    0,
                )
            };
            if rc > 0 {
                filled += rc as usize;
            } else if rc == 0 {
                break;
            } else {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
        }
        return filled == buf.len();
    }
    #[allow(unreachable_code)]
    false
}

// ----- base64 (standard alphabet, matches Python base64.b64encode/b64decode) -----

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 encode with `=` padding, byte-for-byte with Python's
/// `base64.b64encode(...).decode()`.
fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut chunks = data.chunks_exact(3);
    for chunk in &mut chunks {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
        out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[(n & 0x3f) as usize] as char);
    }
    let rest = chunks.remainder();
    match rest.len() {
        1 => {
            let n = (rest[0] as u32) << 16;
            out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rest[0] as u32) << 16) | ((rest[1] as u32) << 8);
            out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(B64_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

/// Map one base64 character to its 6-bit value, or `None` if it is not in the
/// standard alphabet.
fn b64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Standard base64 decode. The QQBot server and [`generate_bind_key`] both emit
/// canonical, padded standard base64, so this is a strict decoder: it requires a
/// length that is a multiple of 4 and rejects any character outside the standard
/// alphabet (aside from the trailing `=` padding). Python's `base64.b64decode`
/// silently discards stray non-alphabet bytes; that leniency is not reproduced
/// because these inputs never contain junk, and being strict fails closed.
fn b64_decode(s: &str) -> Result<Vec<u8>, CryptoError> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(CryptoError::InvalidBase64);
    }
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let chunks = bytes.chunks_exact(4);
    let n_chunks = bytes.len() / 4;
    for (i, chunk) in chunks.enumerate() {
        let is_last = i == n_chunks - 1;
        let pad = if is_last {
            (chunk[3] == b'=') as usize + (chunk[2] == b'=') as usize
        } else {
            0
        };
        // Padding is only ever the last one or two bytes of the final chunk.
        if !is_last && chunk.contains(&b'=') {
            return Err(CryptoError::InvalidBase64);
        }
        if pad == 2 && chunk[2] != b'=' {
            return Err(CryptoError::InvalidBase64);
        }
        let v0 = b64_value(chunk[0]).ok_or(CryptoError::InvalidBase64)?;
        let v1 = b64_value(chunk[1]).ok_or(CryptoError::InvalidBase64)?;
        let v2 = if pad >= 1 && chunk[2] == b'=' {
            0
        } else {
            b64_value(chunk[2]).ok_or(CryptoError::InvalidBase64)?
        };
        let v3 = if pad >= 1 && chunk[3] == b'=' {
            0
        } else {
            b64_value(chunk[3]).ok_or(CryptoError::InvalidBase64)?
        };
        let n = ((v0 as u32) << 18) | ((v1 as u32) << 12) | ((v2 as u32) << 6) | (v3 as u32);
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

// ----- AES-256-GCM open (the missing primitive) -----

/// Open (decrypt and authenticate) an AES-256-GCM ciphertext with no additional
/// authenticated data, returning the plaintext bytes.
///
/// The `ct_with_tag` argument is the ciphertext with the 16-byte GCM tag
/// appended, exactly as `cryptography`'s `AESGCM.decrypt` expects it. Backed by
/// the RustCrypto `aes-gcm` crate; the AEAD is never hand-rolled.
fn aes256gcm_open(key: &[u8], iv: &[u8], ct_with_tag: &[u8]) -> Result<Vec<u8>, CryptoError> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};

    // A 256-bit key and a 96-bit nonce are structural invariants of AES-256-GCM;
    // a mismatch means the inputs were malformed, so fail as a decrypt failure
    // (Python would raise here too, feeding a short nonce to AESGCM.decrypt).
    let key = Key::<Aes256Gcm>::try_from(key).map_err(|_| CryptoError::Decrypt)?;
    let nonce = Nonce::<<Aes256Gcm as aes_gcm::AeadCore>::NonceSize>::try_from(iv)
        .map_err(|_| CryptoError::Decrypt)?;
    let cipher = Aes256Gcm::new(&key);
    cipher
        .decrypt(&nonce, ct_with_tag) // ciphertext || 16-byte tag
        .map_err(|_| CryptoError::Decrypt)
}

// ----- public API -----

/// Generate a 256-bit random AES key and return it as standard base64.
///
/// Mirrors Python's `base64.b64encode(os.urandom(32)).decode()`. The key is
/// passed to `create_bind_task` so the server can encrypt the bot's
/// `client_secret` before returning it. Only this process holds the key, so the
/// secret never travels in plaintext.
///
/// Unlike Python (which raises on CSPRNG failure), this returns
/// [`CryptoError::RandomUnavailable`] so callers fail closed rather than mint a
/// guessable key.
pub fn generate_bind_key() -> Result<String, CryptoError> {
    let mut key = [0u8; KEY_LEN];
    if !fill_random(&mut key) {
        return Err(CryptoError::RandomUnavailable);
    }
    Ok(b64_encode(&key))
}

/// Decrypt a base64-encoded AES-256-GCM ciphertext.
///
/// Ciphertext layout after base64-decoding:
///
/// ```text
/// IV (12 bytes) || ciphertext (N bytes) || AuthTag (16 bytes)
/// ```
///
/// `encrypted_base64` is the `bot_encrypt_secret` value from `poll_bind_result`;
/// `key_base64` is the base64 AES key from [`generate_bind_key`]. Returns the
/// decrypted `client_secret` as a UTF-8 string. There is no additional
/// authenticated data.
pub fn decrypt_secret(encrypted_base64: &str, key_base64: &str) -> Result<String, CryptoError> {
    let key = b64_decode(key_base64)?;
    let raw = b64_decode(encrypted_base64)?;

    // Python slices unconditionally: iv = raw[:12], ciphertext_with_tag = raw[12:].
    // If raw is shorter than the IV, hand what there is to the primitive and let
    // the AEAD open fail, matching Python's behavior of feeding a short nonce to
    // AESGCM.decrypt (which then errors).
    let split = raw.len().min(IV_LEN);
    let iv = &raw[..split];
    let ciphertext_with_tag = &raw[split..];

    let plaintext = aes256gcm_open(&key, iv, ciphertext_with_tag)?;
    String::from_utf8(plaintext).map_err(|_| CryptoError::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors produced by running the real Python module against
    // `cryptography` 50.0.1:
    //
    //   cd /home/eins0fx/development/hermes-agent-port
    //   (build ct = AESGCM(key).encrypt(iv, plaintext, None); raw = iv || ct;
    //    encrypted_base64 = b64encode(raw); key_base64 = b64encode(key))
    //   assert crypto.decrypt_secret(encrypted_base64, key_base64) == plaintext
    //
    // Each tuple is (key_base64, encrypted_base64, plaintext).
    const VECTORS: &[(&str, &str, &str)] = &[
        (
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
            "AAECAwQFBgcICQoLL2e6d6rIsX7uM/L/DeMCKy8+Lerg/b7GjaOWOA==",
            "hello-secret",
        ),
        (
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "ERERERERERERERERVQzn+t+UUriuBcXid76bGA==",
            "",
        ),
        (
            "AAcOFRwjKjE4P0ZNVFtiaXB3foWMk5qhqK+2vcTL0tk=",
            "AA0aJzRBTltodYKPKwIB9hF8x96TlK50UFTU71chDous0NqPv3io5SdacTnwKZonplca1zylnPt7AVfd6Q==",
            "client_secret_ABCdef0123456789!@#",
        ),
        (
            "qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqo=",
            "u7u7u7u7u7u7u7u7so0Mr6ve2X1S3sZ6ASofOsQ=",
            "x",
        ),
    ];

    #[test]
    fn b64_encode_matches_python() {
        // key of case 0 is the byte sequence 0..32; Python b64encode of it.
        let key0: Vec<u8> = (0u8..32).collect();
        assert_eq!(
            b64_encode(&key0),
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="
        );
        // Two-byte tail (needs one '=').
        assert_eq!(b64_encode(&[0x11, 0x22]), "ESI=");
        // One-byte tail (needs two '=').
        assert_eq!(b64_encode(&[0x11]), "EQ==");
        // Empty.
        assert_eq!(b64_encode(&[]), "");
    }

    #[test]
    fn b64_decode_inverts_encode() {
        let key0: Vec<u8> = (0u8..32).collect();
        assert_eq!(
            b64_decode("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap(),
            key0
        );
        assert_eq!(b64_decode("ESI=").unwrap(), vec![0x11, 0x22]);
        assert_eq!(b64_decode("EQ==").unwrap(), vec![0x11]);
        assert_eq!(b64_decode("").unwrap(), Vec::<u8>::new());
        // Round trip a range of lengths.
        for len in 0..40usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let round = b64_decode(&b64_encode(&data)).unwrap();
            assert_eq!(round, data, "round trip failed at len {len}");
        }
    }

    #[test]
    fn b64_decode_rejects_bad_input() {
        assert_eq!(b64_decode("abc"), Err(CryptoError::InvalidBase64)); // not multiple of 4
        assert_eq!(b64_decode("****"), Err(CryptoError::InvalidBase64)); // bad chars
        assert_eq!(b64_decode("a==="), Err(CryptoError::InvalidBase64)); // triple pad
        assert_eq!(b64_decode("a=b="), Err(CryptoError::InvalidBase64)); // pad in wrong spot
    }

    #[test]
    fn vectors_decode_to_expected_layout() {
        // We cannot run the AEAD open yet, but every non-crypto step is locked:
        // the base64 decode, the 12-byte IV split, and the 16-byte tag tail.
        for (key_b64, enc_b64, _pt) in VECTORS {
            let key = b64_decode(key_b64).unwrap();
            assert_eq!(key.len(), KEY_LEN, "key must be 32 bytes");
            let raw = b64_decode(enc_b64).unwrap();
            // IV (12) + tag (16) is the floor; ciphertext length is plaintext length.
            assert!(raw.len() >= IV_LEN + 16, "raw too short: {}", raw.len());
            let iv = &raw[..IV_LEN];
            let ct_with_tag = &raw[IV_LEN..];
            assert_eq!(iv.len(), IV_LEN);
            assert!(
                ct_with_tag.len() >= 16,
                "ciphertext+tag must include the 16-byte tag"
            );
        }
    }

    #[test]
    fn case0_iv_is_first_twelve_bytes() {
        // Case 0 was encrypted with iv = bytes(range(12)); confirm the split
        // recovers exactly those bytes from the base64 blob.
        let raw = b64_decode(VECTORS[0].1).unwrap();
        let expected_iv: Vec<u8> = (0u8..12).collect();
        assert_eq!(&raw[..IV_LEN], &expected_iv[..]);
    }

    #[test]
    fn decrypt_rejects_tampered_ciphertext() {
        // Flipping a byte of the ciphertext must fail the GCM auth tag rather
        // than return a wrong secret. This is the fail-closed guarantee.
        let (key_b64, enc_b64, _pt) = VECTORS[0];
        let mut raw = b64_decode(enc_b64).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        let tampered = b64_encode(&raw);
        assert_eq!(
            decrypt_secret(&tampered, key_b64),
            Err(CryptoError::Decrypt)
        );
    }

    #[test]
    fn decrypt_rejects_wrong_key() {
        // A valid but wrong key must fail the auth tag, never yield plaintext.
        let (_key_b64, enc_b64, _pt) = VECTORS[0];
        let wrong_key = b64_encode(&[0x11u8; KEY_LEN]);
        assert_eq!(
            decrypt_secret(enc_b64, &wrong_key),
            Err(CryptoError::Decrypt)
        );
    }

    #[test]
    fn generate_bind_key_shape() {
        // 32 random bytes -> 44 base64 chars (with one '=' pad), decoding back
        // to exactly 32 bytes. Two calls must differ.
        let k1 = generate_bind_key().expect("CSPRNG should be available in tests");
        let k2 = generate_bind_key().expect("CSPRNG should be available in tests");
        assert_eq!(k1.len(), 44);
        assert!(k1.ends_with('='));
        assert_eq!(b64_decode(&k1).unwrap().len(), KEY_LEN);
        assert_ne!(k1, k2, "two generated keys must differ");
    }

    // Locked golden vectors for the full round trip. Asserts the Rust port
    // reproduces the real Python (cryptography AESGCM) output byte for byte.
    #[test]
    fn decrypt_matches_python_golden_vectors() {
        for (key_b64, enc_b64, pt) in VECTORS {
            assert_eq!(decrypt_secret(enc_b64, key_b64).unwrap(), *pt);
        }
    }
}
