//! Authenticated encryption for credentials at rest.
//!
//! The dispatcher runs long after the HTTP request that created a session,
//! so the caller's credential has to be persisted between the two. Storing
//! a user's platform bearer in plaintext in Postgres would make a database
//! dump a credential dump, so every stored credential goes through here.
//!
//! XChaCha20-Poly1305 with a random 24-byte nonce per value: the extended
//! nonce is large enough that random generation needs no counter and no
//! coordination between controld instances. The nonce is prepended to the
//! ciphertext, so a stored value is self-contained.

use anyhow::{anyhow, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{AeadCore, XChaCha20Poly1305, XNonce};

const NONCE_LEN: usize = 24;

#[derive(Clone)]
pub struct SecretBox {
    cipher: XChaCha20Poly1305,
}

// Never let the key reach a log line or a panic message.
impl std::fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBox(<key redacted>)")
    }
}

impl SecretBox {
    /// Build from `PUKU_SECRET_KEY`: 64 hex chars (32 bytes).
    ///
    /// Returns `Ok(None)` when unset — a dev box then runs with no stored
    /// credentials at all, which is safer than inventing a default key that
    /// would make the ciphertext decryptable by anyone with the source.
    pub fn from_env(key_hex: Option<&str>) -> Result<Option<Self>> {
        let Some(key_hex) = key_hex.map(str::trim).filter(|k| !k.is_empty()) else {
            return Ok(None);
        };
        let bytes = hex::decode(key_hex)
            .context("PUKU_SECRET_KEY must be hex (generate: openssl rand -hex 32)")?;
        if bytes.len() != 32 {
            anyhow::bail!(
                "PUKU_SECRET_KEY must decode to 32 bytes, got {}",
                bytes.len()
            );
        }
        Ok(Some(SecretBox {
            cipher: XChaCha20Poly1305::new(bytes.as_slice().into()),
        }))
    }

    pub fn encrypt(&self, plaintext: &str) -> Result<Vec<u8>> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let mut out = nonce.to_vec();
        let ct = self
            .cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| anyhow!("encryption failed"))?;
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn decrypt(&self, stored: &[u8]) -> Result<String> {
        if stored.len() <= NONCE_LEN {
            anyhow::bail!("stored credential is truncated");
        }
        let (nonce, ct) = stored.split_at(NONCE_LEN);
        let plain = self
            .cipher
            .decrypt(XNonce::from_slice(nonce), ct)
            // A rotated key lands here. Say which failure it is without
            // echoing any of the ciphertext.
            .map_err(|_| anyhow!("credential does not decrypt with the current PUKU_SECRET_KEY"))?;
        String::from_utf8(plain).context("decrypted credential is not utf-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn boxed() -> SecretBox {
        SecretBox::from_env(Some(KEY)).unwrap().unwrap()
    }

    #[test]
    fn round_trips() {
        let b = boxed();
        let ct = b.encrypt("pk_live_secret").unwrap();
        assert_eq!(b.decrypt(&ct).unwrap(), "pk_live_secret");
    }

    /// The plaintext must not be recoverable by eye from the stored bytes.
    #[test]
    fn ciphertext_does_not_contain_the_plaintext() {
        let ct = boxed().encrypt("pk_live_secret").unwrap();
        assert!(!String::from_utf8_lossy(&ct).contains("pk_live_secret"));
    }

    /// A fresh nonce per value: encrypting the same credential twice must
    /// not produce the same bytes, or the store leaks which users share one.
    #[test]
    fn same_plaintext_encrypts_differently_each_time() {
        let b = boxed();
        assert_ne!(b.encrypt("same").unwrap(), b.encrypt("same").unwrap());
    }

    /// Poly1305 is the point: a tampered row must fail, not decrypt to junk.
    #[test]
    fn tampering_is_detected() {
        let b = boxed();
        let mut ct = b.encrypt("pk_live_secret").unwrap();
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        assert!(b.decrypt(&ct).is_err());
    }

    #[test]
    fn a_different_key_cannot_decrypt() {
        let ct = boxed().encrypt("pk_live_secret").unwrap();
        let other = SecretBox::from_env(Some(
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        ))
        .unwrap()
        .unwrap();
        let err = other.decrypt(&ct).unwrap_err().to_string();
        assert!(err.contains("PUKU_SECRET_KEY"), "{err}");
    }

    #[test]
    fn truncated_values_are_rejected_not_panicked_on() {
        assert!(boxed().decrypt(&[0u8; 8]).is_err());
    }

    /// Misconfiguration must fail loudly at boot, not silently disable
    /// encryption or half-work.
    #[test]
    fn bad_keys_are_refused() {
        assert!(SecretBox::from_env(Some("nothex")).is_err());
        assert!(SecretBox::from_env(Some("aabb")).is_err()); // right charset, wrong length
        assert!(SecretBox::from_env(None).unwrap().is_none());
        assert!(SecretBox::from_env(Some("   ")).unwrap().is_none());
    }
}

/// The cross-language fixture, verified from BOTH sides.
///
/// puku-memory-service ports this file to Go and stores credentials in the
/// same format. "Same primitive" is a claim a comment cannot keep: nonce
/// placement, AEAD parameters and key derivation could drift silently and
/// nothing would fail until a real credential failed to decrypt in production.
///
/// So one value, encrypted once by this implementation, is decrypted by both.
/// The Go half lives in internal/secretbox/secretbox_test.go with the same
/// bytes. Regenerate with:
///   cargo test -p puku-controld secretbox::xlang -- --ignored --nocapture
#[cfg(test)]
mod xlang {
    use super::*;

    pub(crate) const KEY: &str =
        "5f0e2b9c1a47d8e36b0f9c2a8d1e4f70a3b6c9d2e5f8a1b4c7d0e3f6a9b2c5d8";
    pub(crate) const PLAINTEXT: &str = "puku-cross-language-fixture-v1";
    pub(crate) const CIPHERTEXT_HEX: &str = "44c00b4ee1a60b1ccaacf6c03f042ec3ba60bdb043d08f1887b4bff1256f7a6056e8eccae5e52f72ac472d1a177829785468d83df32243ac2891c6401eefa42dab9235aab0b1";

    #[test]
    fn the_shared_fixture_decrypts_here_too() {
        let b = SecretBox::from_env(Some(KEY)).unwrap().unwrap();
        let ct = hex::decode(CIPHERTEXT_HEX).unwrap();
        assert_eq!(b.decrypt(&ct).unwrap(), PLAINTEXT);
    }

    /// Run with `--ignored --nocapture` to mint a replacement.
    #[test]
    #[ignore]
    fn print_fixture() {
        let b = SecretBox::from_env(Some(KEY)).unwrap().unwrap();
        println!("FIXTURE_CT={}", hex::encode(b.encrypt(PLAINTEXT).unwrap()));
    }
}
