//! At-rest encryption for persisted secrets (provider upstream api-keys).
//!
//! Boundary module: the DB stores ciphertext; `hydra_core::ProviderKey` holds
//! plaintext only in memory and is never persisted as plaintext. Pure-crypto
//! dependencies (aes-gcm / base64 / rand) live here, never in hydra-core.
//!
//! Algorithm: AES-256-GCM. A fresh 96-bit random nonce is generated per seal.
//! `key_version` is bound as GCM additional authenticated data (AAD), so a
//! ciphertext sealed under one key version fails to open under another.

use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use base64::Engine;
use rand::RngCore;
use std::path::Path;
use thiserror::Error;

/// AES-256 key length, in bytes.
pub const KEY_LEN: usize = 32;
/// GCM nonce length, in bytes (96 bits).
pub const NONCE_LEN: usize = 12;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("master key must be {expected} bytes, got {got}")]
    KeyLength { expected: usize, got: usize },
    #[error("master key is not configured: set HYDRA_ENCRYPTION_KEY (base64 of 32 bytes) or HYDRA_ENCRYPTION_KEY_FILE")]
    KeyMissing,
    #[error("master key is not valid base64: {0}")]
    KeyEncoding(#[from] base64::DecodeError),
    #[error("could not read master key file {path}: {source}")]
    KeyFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("stored key_version {stored} does not match provider key_version {provider}; re-enter the key or rotate")]
    KeyVersionMismatch { stored: u32, provider: u32 },
    #[error("decryption failed (wrong master key or tampered ciphertext)")]
    Decrypt,
    #[error("decrypted key is not valid UTF-8")]
    NotUtf8,
}

/// One encrypted secret as persisted: ciphertext + nonce + key version.
#[derive(Debug, Clone)]
pub struct Sealed {
    pub ciphertext: Vec<u8>,
    pub nonce: [u8; NONCE_LEN],
    pub key_version: u32,
}

/// Abstraction over master-key sources. `StaticKeyProvider` reads the key from
/// the environment; a future `KmsKeyProvider` (AWS KMS / HashiCorp Vault) will
/// implement this same trait without changing call sites.
///
/// TODO(kms): implement `KmsKeyProvider` backed by AWS KMS / HashiCorp Vault.
pub trait KeyProvider: Send + Sync {
    fn seal(&self, plaintext: &[u8]) -> Result<Sealed, CryptoError>;
    fn open(&self, sealed: &Sealed) -> Result<Vec<u8>, CryptoError>;
}

/// Master key from `HYDRA_ENCRYPTION_KEY` (base64, 32 bytes) or
/// `HYDRA_ENCRYPTION_KEY_FILE` (raw 32-byte file). AES-256-GCM, fresh nonce
/// per seal, `key_version` bound as AAD.
pub struct StaticKeyProvider {
    key: [u8; KEY_LEN],
    version: u32,
}

impl StaticKeyProvider {
    /// Construct from an already-loaded raw 32-byte key and a version tag.
    pub fn new(key: [u8; KEY_LEN], version: u32) -> Self {
        Self { key, version }
    }

    /// Current master-key version tag.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Load from the environment, preferring the file form. Returns
    /// `CryptoError::KeyMissing` (fail-closed) when neither var is set.
    pub fn from_env() -> Result<Self, CryptoError> {
        let raw = if let Some(p) = std::env::var_os("HYDRA_ENCRYPTION_KEY_FILE") {
            let path = Path::new(&p);
            let bytes = std::fs::read(path).map_err(|e| CryptoError::KeyFile {
                path: path.display().to_string(),
                source: e,
            })?;
            trim_key_bytes(bytes)
        } else if let Some(b64) = std::env::var_os("HYDRA_ENCRYPTION_KEY") {
            let s = b64.to_string_lossy().into_owned();
            base64::engine::general_purpose::STANDARD
                .decode(s.trim())
                .map_err(CryptoError::KeyEncoding)?
        } else {
            return Err(CryptoError::KeyMissing);
        };
        if raw.len() != KEY_LEN {
            return Err(CryptoError::KeyLength {
                expected: KEY_LEN,
                got: raw.len(),
            });
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&raw);
        Ok(Self::new(key, 1))
    }
}

impl KeyProvider for StaticKeyProvider {
    fn seal(&self, plaintext: &[u8]) -> Result<Sealed, CryptoError> {
        let cipher = Aes256Gcm::new(&self.key.into());
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad: &self.version.to_le_bytes(),
                },
            )
            .map_err(|_| CryptoError::Decrypt)?;
        Ok(Sealed {
            ciphertext: ct,
            nonce: nonce_bytes,
            key_version: self.version,
        })
    }

    fn open(&self, sealed: &Sealed) -> Result<Vec<u8>, CryptoError> {
        // `key_version` is also bound as AAD below, so a version mismatch would
        // fail the GCM tag check. We surface a clearer error up-front.
        if sealed.key_version != self.version {
            return Err(CryptoError::KeyVersionMismatch {
                stored: sealed.key_version,
                provider: self.version,
            });
        }
        let cipher = Aes256Gcm::new(&self.key.into());
        let nonce = Nonce::from_slice(&sealed.nonce);
        cipher
            .decrypt(
                nonce,
                Payload {
                    msg: &sealed.ciphertext,
                    aad: &sealed.key_version.to_le_bytes(),
                },
            )
            .map_err(|_| CryptoError::Decrypt)
    }
}

/// Trim a single trailing newline (and optional CR) so `echo -n` vs `echo`
/// both produce the same raw key bytes.
fn trim_key_bytes(mut b: Vec<u8>) -> Vec<u8> {
    if matches!(b.last(), Some(b'\n')) {
        b.pop();
    }
    if matches!(b.last(), Some(b'\r')) {
        b.pop();
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kp() -> StaticKeyProvider {
        StaticKeyProvider::new([7u8; KEY_LEN], 1)
    }

    #[test]
    fn round_trip() {
        let kp = kp();
        let sealed = kp.seal(b"sk-provider-secret-123").unwrap();
        assert_eq!(kp.open(&sealed).unwrap(), b"sk-provider-secret-123");
    }

    #[test]
    fn nonce_is_unique_per_seal() {
        let kp = kp();
        let a = kp.seal(b"same").unwrap();
        let b = kp.seal(b"same").unwrap();
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let kp = kp();
        let mut sealed = kp.seal(b"secret").unwrap();
        sealed.ciphertext[0] ^= 0xff;
        assert!(matches!(kp.open(&sealed), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn wrong_master_key_is_rejected() {
        let seal_kp = StaticKeyProvider::new([1u8; KEY_LEN], 1);
        let open_kp = StaticKeyProvider::new([2u8; KEY_LEN], 1);
        let sealed = seal_kp.seal(b"secret").unwrap();
        // version matches (both 1) but key differs -> tag failure
        assert!(matches!(open_kp.open(&sealed), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let seal_kp = StaticKeyProvider::new([1u8; KEY_LEN], 1);
        let open_kp = StaticKeyProvider::new([1u8; KEY_LEN], 2);
        let sealed = seal_kp.seal(b"secret").unwrap();
        assert!(matches!(
            open_kp.open(&sealed),
            Err(CryptoError::KeyVersionMismatch { .. })
        ));
    }
}
