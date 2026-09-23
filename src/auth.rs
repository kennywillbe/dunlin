//! Password hashing and constant-time token helpers.

use anyhow::{bail, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::RngCore;
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
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
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

    #[test]
    fn constant_time_eq() {
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "abd"));
        assert!(!secret_eq("abc", "abcd"));
    }
}
