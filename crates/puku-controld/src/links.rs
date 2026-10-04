//! Capability URLs: a way into one port of one machine without a bearer.
//!
//! A browser iframe cannot send an `Authorization` header, so a noVNC view
//! is shared through a URL whose path carries a signed, expiring token
//! instead. The token names the machine and the port and nothing else, so a
//! view link can never be widened into a control link: that distinction is a
//! different port, and a different token.
//!
//! Format: `<machine 32 hex>.<port>.<expires unix secs>.<hmac 32 hex>`.
//! Every character is URL-safe, so it sits in a path segment untouched.

use sha2::{Digest, Sha256};
use uuid::Uuid;

pub struct LinkSigner {
    key: [u8; 32],
}

impl std::fmt::Debug for LinkSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LinkSigner(..)")
    }
}

impl LinkSigner {
    /// Domain-separated from whatever else `secret` is used for.
    pub fn new(secret: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(b"puku-links-v1\0");
        h.update(secret);
        LinkSigner { key: h.finalize().into() }
    }

    pub fn mint(&self, machine_id: Uuid, port: u16, expires_at: i64) -> String {
        let payload = format!("{}.{port}.{expires_at}", machine_id.simple());
        let sig = hmac_sha256(&self.key, payload.as_bytes());
        format!("{payload}.{}", hex::encode(&sig[..16]))
    }

    /// The machine and port a capability grants, if it is genuine and has
    /// not expired at `now` (unix seconds).
    pub fn verify(&self, cap: &str, now: i64) -> Option<(Uuid, u16)> {
        let mut parts = cap.split('.');
        let (id, port, exp, sig) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        let payload = format!("{id}.{port}.{exp}");
        let want = hmac_sha256(&self.key, payload.as_bytes());
        let got = hex::decode(sig).ok()?;
        if got.len() != 16 || !constant_time_eq(&got, &want[..16]) {
            return None;
        }
        let exp: i64 = exp.parse().ok()?;
        if exp < now {
            return None;
        }
        Some((Uuid::parse_str(id).ok()?, port.parse().ok()?))
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// HMAC-SHA256 (RFC 2104), on the sha2 this crate already carries rather
/// than a second digest stack.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231 test case 2, so the hand-rolled HMAC is the real one.
    #[test]
    fn hmac_matches_rfc_4231() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// Test case 6: a key longer than the block is hashed first.
    #[test]
    fn hmac_handles_long_keys() {
        let mac = hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First");
        assert_eq!(
            hex::encode(mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn a_minted_link_verifies_until_it_expires() {
        let s = LinkSigner::new(b"secret");
        let id = Uuid::new_v4();
        let cap = s.mint(id, 6080, 1000);
        assert_eq!(s.verify(&cap, 999), Some((id, 6080)));
        assert_eq!(s.verify(&cap, 1000), Some((id, 6080)));
        assert_eq!(s.verify(&cap, 1001), None, "expired");
    }

    /// The port is signed: a view link cannot be edited into a control link.
    #[test]
    fn tampering_with_any_field_breaks_it() {
        let s = LinkSigner::new(b"secret");
        let id = Uuid::new_v4();
        let cap = s.mint(id, 6080, 1000);
        let control = cap.replacen(".6080.", ".6081.", 1);
        assert_eq!(s.verify(&control, 0), None);
        let later = cap.replacen(".1000.", ".9999.", 1);
        assert_eq!(s.verify(&later, 0), None);
        assert_eq!(LinkSigner::new(b"other").verify(&cap, 0), None, "another key");
        assert_eq!(s.verify("garbage", 0), None);
        assert_eq!(s.verify(&format!("{cap}.extra"), 0), None);
    }

    #[test]
    fn links_are_url_safe() {
        let cap = LinkSigner::new(b"k").mint(Uuid::new_v4(), 1, 2);
        assert!(cap.chars().all(|c| c.is_ascii_hexdigit() || c == '.' || c.is_ascii_digit()));
    }
}
