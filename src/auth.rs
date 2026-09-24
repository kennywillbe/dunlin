//! Password hashing and constant-time token helpers.

use anyhow::{bail, Result};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use rand::Rng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Minimum password length enforced by `dunlin hash-password`.
pub const MIN_PASSWORD_LEN: usize = 12;

pub fn hash_password(password: &str) -> Result<String> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        bail!("password must be at least {MIN_PASSWORD_LEN} characters");
    }
    let mut salt_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut salt_bytes);
    let hash = Argon2::default()
        .hash_password_with_salt(password.as_bytes(), &salt_bytes)
        .map_err(|e| anyhow::anyhow!("hashing failed: {e}"))?
        .to_string();
    Ok(hash)
}

pub fn verify_password(password: &str, phc: &str) -> Result<bool> {
    let parsed = PasswordHash::new(phc)
        .map_err(|e| anyhow::anyhow!("invalid argon2 hash in config: {e}"))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

/// Random 32-byte token, hex encoded.
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// HMAC-SHA256 (RFC 2104), for tokens that are derived rather than stored.
/// A few lines over `sha2` instead of another crate; the tests pin it to the
/// RFC 4231 vectors.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner.finalize());
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer.finalize());
    out
}

/// Constant-time comparison of a configured secret against a supplied value.
pub fn secret_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let h = hash_password("correcthorsebattery").unwrap();
        assert!(verify_password("correcthorsebattery", &h).unwrap());
        assert!(!verify_password("wrong", &h).unwrap());
    }

    // Hashes written by earlier releases (argon2 0.5) live in users' configs;
    // this is the one shipped in dunlin.example.toml.
    #[test]
    fn verifies_existing_phc_hash() {
        let phc = "$argon2id$v=19$m=19456,t=2,p=1$HAxHiOcZJRfd5p0TbNMaUA$WaS5hjKfMZye0zcHnrwWqsUu3nWHgBWqzO/9D46dUYo";
        assert!(verify_password("changemechangeme", phc).unwrap());
        assert!(!verify_password("changemechangemf", phc).unwrap());
    }

    #[test]
    fn new_hash_is_default_argon2id_phc() {
        let h = hash_password("correcthorsebattery").unwrap();
        assert!(h.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{h}");
        assert!(PasswordHash::new(&h).is_ok());
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        let h = |key: &[u8], msg: &[u8]| hex::encode(hmac_sha256(key, msg));
        assert_eq!(
            h(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            h(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Case 6: a key longer than the block is hashed first.
        assert_eq!(
            h(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            ),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn min_length() {
        assert!(hash_password("short").is_err());
        assert!(hash_password("123456789012").is_ok());
    }

    #[test]
    fn token_is_random_and_hashed() {
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(token_hash(&a), token_hash(&b));
        assert_eq!(token_hash(&a).len(), 64);
    }

    // Session rows store this hash, so a change in its encoding would log
    // everyone out after an upgrade.
    #[test]
    fn token_hash_is_lowercase_hex_sha256() {
        assert_eq!(
            token_hash("dunlin-session-token"),
            "081738e774805599d2e0b4c3c9659b09ddddda8c80acbe1bdcec025f15666214"
        );
    }

    #[test]
    fn constant_time_eq() {
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "abd"));
        assert!(!secret_eq("abc", "abcd"));
    }
}
