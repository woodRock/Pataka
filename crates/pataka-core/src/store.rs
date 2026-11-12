//! Encrypted local storage.
//!
//! Layout under the app data directory:
//!
//! ```text
//! keyring.json            how the data key is held (keychain or passphrase), never the key itself
//! settings.ptk            encrypted settings, providers and API keys
//! mail.ptk                encrypted Pātaka Mail account and label-name table
//! korero/<uuid>.ptk       one encrypted file per kōrero
//! audit.jsonl             hash-chained audit log (counts only, no message text)
//! ```
//!
//! Writes go to a temporary file that is then renamed, so a crash cannot
//! leave a half-written file.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::crypto::{self, DataKey, KdfParams};
use crate::error::{Error, Result};
use crate::model::{Conversation, Settings};

const KEYCHAIN_SERVICE: &str = "nz.pataka.desktop";
const KEYCHAIN_USER: &str = "data-key";
const CHECK_AAD: &[u8] = b"pataka:key-check";
const WRAP_AAD: &[u8] = b"pataka:wrapped-key";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyMode {
    Keychain,
    Passphrase,
}

#[derive(Serialize, Deserialize)]
struct KeyringFile {
    version: u32,
    mode: KeyMode,
    /// Passphrase mode: Argon2id salt and settings, and the wrapped data key.
    #[serde(default)]
    salt: Option<String>,
    #[serde(default)]
    kdf: Option<KdfParams>,
    #[serde(default)]
    wrapped: Option<String>,
    /// Keychain mode: a known value sealed with the data key, to detect a wrong key.
    #[serde(default)]
    check: Option<String>,
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| Error::Io("bad path".into()))?;
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    restrict(&tmp);
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Owner-only permissions where the OS supports them.
#[cfg(unix)]
pub(crate) fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
pub(crate) fn restrict(_path: &Path) {}

/// Overwrite a file with zeros before removing it. On SSDs and copy-on-write
/// file systems this is best effort; encryption is the real protection.
fn shred(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::metadata(path) {
        if let Ok(mut f) = fs::OpenOptions::new().write(true).open(path) {
            let zeros = vec![0u8; meta.len() as usize];
            let _ = f.write_all(&zeros);
            let _ = f.sync_all();
        }
        fs::remove_file(path)?;
    }
    Ok(())
}

fn valid_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| Error::Invalid("bad id".into()))
}

fn keychain_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER).map_err(|e| Error::Keychain(e.to_string()))
}

impl Store {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("korero"))?;
        restrict(&root);
        restrict(&root.join("korero"));
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn keyring_path(&self) -> PathBuf {
        self.root.join("keyring.json")
    }

    pub fn audit_path(&self) -> PathBuf {
        self.root.join("audit.jsonl")
    }

    fn conv_path(&self, id: &str) -> PathBuf {
        self.root.join("korero").join(format!("{id}.ptk"))
    }

    fn read_keyring(&self) -> Result<Option<KeyringFile>> {
        match fs::read(self.keyring_path()) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn key_mode(&self) -> Result<Option<KeyMode>> {
        Ok(self.read_keyring()?.map(|k| k.mode))
    }

    /// First run: create the data key and store it the chosen way.
    pub fn initialise(&self, mode: KeyMode, passphrase: Option<&str>) -> Result<DataKey> {
        if self.read_keyring()?.is_some() {
            return Err(Error::AlreadyInitialised);
        }
        let key = DataKey::generate();
        let file = match mode {
            KeyMode::Passphrase => {
                let pass = passphrase.ok_or_else(|| Error::Invalid("a passphrase is required".into()))?;
                check_passphrase_strength(pass)?;
                let salt = crypto::random_salt();
                let kdf = KdfParams::default();
                let kek = crypto::derive_kek(pass, &salt, kdf)?;
                let wrapped = crypto::seal(kek.as_ref(), WRAP_AAD, key.as_bytes());
                KeyringFile {
                    version: 1,
                    mode,
                    salt: Some(B64.encode(salt)),
                    kdf: Some(kdf),
                    wrapped: Some(B64.encode(wrapped)),
                    check: None,
                }
            }
            KeyMode::Keychain => {
                let encoded = Zeroizing::new(B64.encode(key.as_bytes()));
                keychain_entry()?
                    .set_password(&encoded)
                    .map_err(|e| Error::Keychain(e.to_string()))?;
                let check = crypto::seal(key.as_bytes(), CHECK_AAD, b"pataka");
                KeyringFile {
                    version: 1,
                    mode,
                    salt: None,
                    kdf: None,
                    wrapped: None,
                    check: Some(B64.encode(check)),
                }
            }
        };
        write_atomic(&self.keyring_path(), &serde_json::to_vec_pretty(&file)?)?;
        self.save_settings(&key, &Settings::default())?;
        Ok(key)
    }

    pub fn unlock(&self, passphrase: Option<&str>) -> Result<DataKey> {
        let file = self.read_keyring()?.ok_or(Error::NotInitialised)?;
        match file.mode {
            KeyMode::Passphrase => {
                let pass = passphrase.ok_or(Error::BadPassphrase)?;
                let salt = B64
                    .decode(file.salt.ok_or(Error::Integrity)?)
                    .map_err(|_| Error::Integrity)?;
                let wrapped = B64
                    .decode(file.wrapped.ok_or(Error::Integrity)?)
                    .map_err(|_| Error::Integrity)?;
                let kek = crypto::derive_kek(pass, &salt, file.kdf.ok_or(Error::Integrity)?)?;
                let raw = crypto::open(kek.as_ref(), WRAP_AAD, &wrapped).map_err(|_| Error::BadPassphrase)?;
                DataKey::from_bytes(&raw)
            }
            KeyMode::Keychain => {
                let encoded = Zeroizing::new(
                    keychain_entry()?
                        .get_password()
                        .map_err(|e| Error::Keychain(e.to_string()))?,
                );
                let raw = Zeroizing::new(B64.decode(encoded.as_bytes()).map_err(|_| Error::Integrity)?);
                let key = DataKey::from_bytes(&raw)?;
                let check = B64
                    .decode(file.check.ok_or(Error::Integrity)?)
                    .map_err(|_| Error::Integrity)?;
                crypto::open(key.as_bytes(), CHECK_AAD, &check)?;
                Ok(key)
            }
        }
    }

    /// Re-wrap the existing data key: switch between keychain and passphrase,
    /// or change the passphrase. Data files are untouched.
    pub fn rekey(&self, key: &DataKey, mode: KeyMode, passphrase: Option<&str>) -> Result<()> {
        let old = self.read_keyring()?.ok_or(Error::NotInitialised)?;
        let file = match mode {
            KeyMode::Passphrase => {
                let pass = passphrase.ok_or_else(|| Error::Invalid("a passphrase is required".into()))?;
                check_passphrase_strength(pass)?;
                let salt = crypto::random_salt();
                let kdf = KdfParams::default();
                let kek = crypto::derive_kek(pass, &salt, kdf)?;
                KeyringFile {
                    version: 1,
                    mode,
                    salt: Some(B64.encode(salt)),
                    kdf: Some(kdf),
                    wrapped: Some(B64.encode(crypto::seal(kek.as_ref(), WRAP_AAD, key.as_bytes()))),
                    check: None,
                }
            }
            KeyMode::Keychain => {
                let encoded = Zeroizing::new(B64.encode(key.as_bytes()));
                keychain_entry()?
                    .set_password(&encoded)
                    .map_err(|e| Error::Keychain(e.to_string()))?;
                KeyringFile {
                    version: 1,
                    mode,
                    salt: None,
                    kdf: None,
                    wrapped: None,
                    check: Some(B64.encode(crypto::seal(key.as_bytes(), CHECK_AAD, b"pataka"))),
                }
            }
        };
        write_atomic(&self.keyring_path(), &serde_json::to_vec_pretty(&file)?)?;
        if old.mode == KeyMode::Keychain && mode == KeyMode::Passphrase {
            // The key must not stay in the keychain once a passphrase protects it.
            if let Ok(e) = keychain_entry() {
                let _ = e.delete_credential();
            }
        }
        Ok(())
    }

    fn seal_json<T: Serialize>(key: &DataKey, aad: &str, value: &T) -> Result<Vec<u8>> {
        let plain = Zeroizing::new(serde_json::to_vec(value)?);
        Ok(crypto::seal(key.as_bytes(), aad.as_bytes(), &plain))
    }

    fn open_json<T: for<'de> Deserialize<'de>>(key: &DataKey, aad: &str, bytes: &[u8]) -> Result<T> {
        let plain = crypto::open(key.as_bytes(), aad.as_bytes(), bytes)?;
        Ok(serde_json::from_slice(&plain)?)
    }

    pub fn load_settings(&self, key: &DataKey) -> Result<Settings> {
        match fs::read(self.root.join("settings.ptk")) {
            Ok(b) => Self::open_json(key, "pataka:settings", &b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_settings(&self, key: &DataKey, s: &Settings) -> Result<()> {
        write_atomic(
            &self.root.join("settings.ptk"),
            &Self::seal_json(key, "pataka:settings", s)?,
        )
    }

    /// Pātaka Mail: the connected account and the label-name table, in
    /// `mail.ptk`. Empty when no account was ever connected.
    pub fn load_mail<T: for<'de> Deserialize<'de> + Default>(&self, key: &DataKey) -> Result<T> {
        match fs::read(self.root.join("mail.ptk")) {
            Ok(b) => Self::open_json(key, "pataka:mail", &b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_mail<T: Serialize>(&self, key: &DataKey, value: &T) -> Result<()> {
        write_atomic(
            &self.root.join("mail.ptk"),
            &Self::seal_json(key, "pataka:mail", value)?,
        )
    }

    pub fn load_conversation(&self, key: &DataKey, id: &str) -> Result<Conversation> {
        valid_id(id)?;
        let bytes = fs::read(self.conv_path(id)).map_err(|_| Error::NotFound("kōrero".into()))?;
        Self::open_json(key, &format!("pataka:korero:{id}"), &bytes)
    }

    pub fn save_conversation(&self, key: &DataKey, c: &Conversation) -> Result<()> {
        valid_id(&c.id)?;
        write_atomic(
            &self.conv_path(&c.id),
            &Self::seal_json(key, &format!("pataka:korero:{}", c.id), c)?,
        )
    }

    pub fn delete_conversation(&self, id: &str) -> Result<()> {
        valid_id(id)?;
        shred(&self.conv_path(id))
    }

    pub fn conversation_ids(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(self.root.join("korero"))? {
            let name = entry?.file_name().to_string_lossy().to_string();
            if let Some(id) = name.strip_suffix(".ptk") {
                if valid_id(id).is_ok() {
                    ids.push(id.to_string());
                }
            }
        }
        Ok(ids)
    }

    /// Remove every file Pātaka wrote and the keychain entry.
    pub fn wipe(&self) -> Result<()> {
        for id in self.conversation_ids()? {
            let _ = self.delete_conversation(&id);
        }
        for f in ["settings.ptk", "mail.ptk", "keyring.json", "audit.jsonl"] {
            let _ = shred(&self.root.join(f));
        }
        if let Ok(e) = keychain_entry() {
            let _ = e.delete_credential();
        }
        Ok(())
    }
}

pub fn check_passphrase_strength(p: &str) -> Result<()> {
    if p.chars().count() < 12 {
        return Err(Error::Invalid(
            "Use a passphrase of at least 12 characters, e.g. four unrelated words.".into(),
        ));
    }
    let distinct: std::collections::BTreeSet<char> = p.chars().collect();
    if distinct.len() < 6 {
        return Err(Error::Invalid(
            "That passphrase repeats too few characters to be safe.".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Classification, Conversation};

    #[test]
    fn passphrase_round_trip_and_wrong_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path().to_path_buf()).unwrap();
        let k = s
            .initialise(KeyMode::Passphrase, Some("correct horse battery"))
            .unwrap();
        assert_eq!(
            s.unlock(Some("correct horse battery")).unwrap().as_bytes(),
            k.as_bytes()
        );
        assert!(matches!(
            s.unlock(Some("wrong horse battery!")),
            Err(Error::BadPassphrase)
        ));
        assert!(matches!(
            s.initialise(KeyMode::Passphrase, Some("another passphrase")),
            Err(Error::AlreadyInitialised)
        ));
    }

    #[test]
    fn conversations_encrypted_and_bound_to_id() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path().to_path_buf()).unwrap();
        let k = s
            .initialise(KeyMode::Passphrase, Some("correct horse battery"))
            .unwrap();
        let id = crate::model::new_id();
        let c = Conversation {
            id: id.clone(),
            title: "Kōrero with Aroha".into(),
            classification: Classification::Tapu,
            created_at: 1,
            updated_at: 1,
            messages: vec![],
            vault: Default::default(),
            terms: vec![],
            allow: vec![],
            agent: None,
        };
        s.save_conversation(&k, &c).unwrap();
        let raw = fs::read(s.conv_path(&id)).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("Aroha"));
        assert_eq!(s.load_conversation(&k, &id).unwrap().title, c.title);
        // A file copied under another id does not decrypt.
        let other = crate::model::new_id();
        fs::copy(s.conv_path(&id), s.conv_path(&other)).unwrap();
        assert!(s.load_conversation(&k, &other).is_err());
        s.delete_conversation(&id).unwrap();
        assert!(s.load_conversation(&k, &id).is_err());
    }

    #[test]
    fn rejects_path_tricks() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path().to_path_buf()).unwrap();
        assert!(s.delete_conversation("../settings").is_err());
    }

    #[test]
    fn weak_passphrases_rejected() {
        assert!(check_passphrase_strength("short").is_err());
        assert!(check_passphrase_strength("aaaaaaaaaaaaaaaa").is_err());
        assert!(check_passphrase_strength("tui kererū kākā ruru").is_ok());
    }
}
