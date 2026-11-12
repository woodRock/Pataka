//! Encryption at rest.
//!
//! One random 256-bit data key encrypts every file Pātaka writes, with
//! XChaCha20-Poly1305 (an AEAD). The associated data binds each ciphertext to
//! its purpose and file name, so files cannot be swapped for one another.
//!
//! The data key itself is kept either in the OS keychain, or wrapped with a
//! key derived from the user's passphrase by Argon2id.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand_core::{OsRng, RngCore};
use zeroize::{Zeroize, Zeroizing};

use crate::error::{Error, Result};

pub const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const MAGIC: &[u8; 4] = b"PTK1";

/// The data key. Zeroed when dropped.
#[derive(Clone)]
pub struct DataKey(Zeroizing<[u8; KEY_LEN]>);

impl DataKey {
    pub fn generate() -> Self {
        let mut k = Zeroizing::new([0u8; KEY_LEN]);
        OsRng.fill_bytes(k.as_mut());
        DataKey(k)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != KEY_LEN {
            return Err(Error::Integrity);
        }
        let mut k = Zeroizing::new([0u8; KEY_LEN]);
        k.copy_from_slice(bytes);
        Ok(DataKey(k))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_ref()
    }
}

/// Encrypt `plain` with `key`; `aad` names what the data is.
pub fn seal(key: &[u8], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("32-byte key");
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
        .expect("encryption cannot fail for in-memory buffers");
    let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

/// Decrypt data produced by [`seal`] with the same key and `aad`.
pub fn open(key: &[u8], aad: &[u8], sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    if sealed.len() < MAGIC.len() + NONCE_LEN + 16 || &sealed[..4] != MAGIC {
        return Err(Error::Integrity);
    }
    let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| Error::Integrity)?;
    let nonce = XNonce::from_slice(&sealed[4..4 + NONCE_LEN]);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: &sealed[4 + NONCE_LEN..],
                aad,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| Error::Integrity)
}

/// Argon2id settings, stored with the wrapped key so they can be raised later.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct KdfParams {
    /// Memory in KiB.
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Default for KdfParams {
    /// 64 MiB, 3 passes: above the OWASP minimums for Argon2id.
    fn default() -> Self {
        KdfParams {
            m_cost: 64 * 1024,
            t_cost: 3,
            p_cost: 1,
        }
    }
}

pub fn random_salt() -> [u8; 16] {
    let mut s = [0u8; 16];
    OsRng.fill_bytes(&mut s);
    s
}

/// Derive a key-encryption key from a passphrase.
pub fn derive_kek(passphrase: &str, salt: &[u8], params: KdfParams) -> Result<Zeroizing<[u8; KEY_LEN]>> {
    let p = Params::new(params.m_cost, params.t_cost, params.p_cost, Some(KEY_LEN))
        .map_err(|_| Error::Invalid("bad key-derivation settings".into()))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, p);
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    argon
        .hash_password_into(passphrase.as_bytes(), salt, out.as_mut())
        .map_err(|_| Error::Invalid("could not derive a key from the passphrase".into()))?;
    Ok(out)
}

/// Overwrite a byte buffer before it is dropped.
pub fn wipe(mut v: Vec<u8>) {
    v.zeroize();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let k = DataKey::generate();
        let s = seal(k.as_bytes(), b"conv:1", b"kia ora");
        assert_eq!(&open(k.as_bytes(), b"conv:1", &s).unwrap()[..], b"kia ora");
    }

    #[test]
    fn wrong_aad_or_key_fails() {
        let k = DataKey::generate();
        let s = seal(k.as_bytes(), b"conv:1", b"kia ora");
        assert!(open(k.as_bytes(), b"conv:2", &s).is_err());
        assert!(open(DataKey::generate().as_bytes(), b"conv:1", &s).is_err());
    }

    #[test]
    fn tamper_fails() {
        let k = DataKey::generate();
        let mut s = seal(k.as_bytes(), b"a", b"kia ora");
        let n = s.len() - 1;
        s[n] ^= 1;
        assert!(open(k.as_bytes(), b"a", &s).is_err());
    }

    #[test]
    fn kdf_is_deterministic() {
        let p = KdfParams {
            m_cost: 1024,
            t_cost: 1,
            p_cost: 1,
        };
        let salt = [7u8; 16];
        assert_eq!(
            *derive_kek("pass phrase", &salt, p).unwrap(),
            *derive_kek("pass phrase", &salt, p).unwrap()
        );
        assert_ne!(
            *derive_kek("pass phrase", &salt, p).unwrap(),
            *derive_kek("pass phrasf", &salt, p).unwrap()
        );
    }
}
