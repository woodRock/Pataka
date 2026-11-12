//! Pātaka Mail: Claude Code reads, searches and organises an IMAP mailbox
//! (Gmail, or any IMAP server) through an MCP server inside the proxy.
//!
//! - The account (address, app password, server) is stored encrypted in
//!   `mail.ptk` under Pātaka's data key.
//! - Tool calls arrive with real values: the proxy restores placeholders in
//!   the arguments of `mcp__pataka-mail__*` tools only, for sessions that
//!   registered the server. Results go back through the proxy, which redacts
//!   them before Anthropic sees them.
//! - Every change is written to the audit log with message ids and hashed
//!   label references, never subjects, addresses or text, and can be undone
//!   with `pataka mail undo <entry>`.

pub mod imap;
pub mod mcp;
pub mod ops;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::crypto::DataKey;
use crate::error::{Error, Result};
use crate::store::Store;

/// Gmail's IMAP server, the default.
pub const GMAIL_HOST: &str = "imap.gmail.com";
pub const IMAPS_PORT: u16 = 993;

/// Name prefix of this server's tools as Claude Code sees them.
pub const TOOL_PREFIX: &str = "mcp__pataka-mail__";
/// The server name given to Claude Code.
pub const SERVER_NAME: &str = "pataka-mail";

fn yes() -> bool {
    true
}

/// A connected mailbox. Not `Debug`, so the password cannot be logged.
#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub email: String,
    pub password: Zeroizing<String>,
    pub host: String,
    pub port: u16,
    /// Always true, except in tests against a server on this computer.
    #[serde(default = "yes")]
    pub tls: bool,
}

impl Account {
    pub fn server(&self) -> imap::Server {
        imap::Server {
            host: self.host.clone(),
            port: self.port,
            tls: self.tls,
        }
    }
}

/// `mail.ptk`: the account, and the table that turns hashed label
/// references in the audit log back into names for `undo`.
#[derive(Default, Serialize, Deserialize)]
pub struct MailFile {
    #[serde(default)]
    pub account: Option<Account>,
    /// Key for the label-name hashes, so they cannot be guessed from a list
    /// of common names.
    #[serde(default)]
    pub salt: String,
    /// Reference → label or folder name.
    #[serde(default)]
    pub names: BTreeMap<String, String>,
}

/// A label or folder name as the audit log holds it: a keyed hash of the
/// name, and a reference into the encrypted name table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NameRef {
    pub hash: String,
    #[serde(rename = "ref")]
    pub reference: String,
}

/// Access to `mail.ptk` with the unlocked data key.
#[derive(Clone)]
pub struct MailVault {
    pub store: Store,
    pub key: DataKey,
}

impl MailVault {
    pub fn load(&self) -> Result<MailFile> {
        self.store.load_mail(&self.key)
    }

    pub fn save(&self, f: &MailFile) -> Result<()> {
        self.store.save_mail(&self.key, f)
    }

    pub fn account(&self) -> Result<Account> {
        self.load()?.account.ok_or_else(|| {
            Error::Invalid("No mailbox is connected. Run `pataka mail connect` in a terminal first.".into())
        })
    }

    pub fn connected(&self) -> Result<bool> {
        Ok(self.load()?.account.is_some())
    }

    pub fn set_account(&self, account: Option<Account>) -> Result<()> {
        let mut f = self.load()?;
        f.account = account;
        self.save(&f)
    }

    /// Hashed references for `names`, remembering each name in the table.
    pub fn refs(&self, names: &[String]) -> Result<Vec<NameRef>> {
        let mut f = self.load()?;
        if f.salt.is_empty() {
            f.salt = crate::agent_proxy::new_token();
        }
        let mut changed = false;
        let out = names
            .iter()
            .map(|n| {
                let hash = name_hash(&f.salt, n);
                let reference = format!("mail.ptk#{hash}");
                if !f.names.contains_key(&reference) {
                    f.names.insert(reference.clone(), n.clone());
                    changed = true;
                }
                NameRef { hash, reference }
            })
            .collect();
        if changed {
            self.save(&f)?;
        }
        Ok(out)
    }

    pub fn name_ref(&self, name: &str) -> Result<NameRef> {
        Ok(self.refs(&[name.to_string()])?.remove(0))
    }

    /// The name behind a reference, checked against its hash.
    pub fn resolve(&self, r: &NameRef) -> Result<String> {
        let f = self.load()?;
        let name = f
            .names
            .get(&r.reference)
            .ok_or_else(|| Error::NotFound("that label in Pātaka's mail store".into()))?;
        if name_hash(&f.salt, name) != r.hash {
            return Err(Error::Integrity);
        }
        Ok(name.clone())
    }
}

fn name_hash(salt: &str, name: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"pataka-mail-name\0");
    h.update(salt.as_bytes());
    h.update([0u8]);
    h.update(name.as_bytes());
    hex::encode(&h.finalize()[..12])
}

/// Label changes on Gmail messages, by Gmail message id (X-GM-MSGID).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelChange {
    pub add: bool,
    pub label: NameRef,
    pub ids: Vec<String>,
}

/// Flag changes (`\Seen`, `\Flagged`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlagChange {
    pub add: bool,
    pub flag: String,
    pub ids: Vec<String>,
}

/// Messages moved between folders. On Gmail `ids` are message ids; on other
/// servers `uids` pairs each old UID with its new one (from COPYUID).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Moved {
    pub from: NameRef,
    pub to: NameRef,
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default)]
    pub uids: Vec<(u32, u32)>,
}

/// Where a saved draft went. Never its recipients, subject or text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drafted {
    pub mailbox: NameRef,
    pub uid: u32,
}

/// What one mutation changed: enough to undo it, and nothing about the
/// messages' content.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Record {
    pub action: String,
    /// Gmail: message ids. Other servers: UIDs in `mailbox`.
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mailbox: Option<NameRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<LabelChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<FlagChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub moved: Vec<Moved>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<NameRef>,
    /// A draft that was saved: its folder and UID there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drafted: Option<Drafted>,
    /// For an undo: the entry it reversed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undoes: Option<u64>,
}

impl Record {
    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
            && self.flags.is_empty()
            && self.moved.is_empty()
            && self.created.is_none()
            && self.drafted.is_none()
    }
}

/// Write a mutation to the audit log. Returns the entry's number.
pub fn audit_record(audit_path: &std::path::Path, korero: Option<&str>, host: &str, record: &Record) -> Result<u64> {
    let n = record.ids.len();
    let e = crate::audit::append(
        audit_path,
        crate::audit::Entry {
            event: format!("mail-{}", record.action),
            korero: korero.map(String::from),
            provider: Some(host.to_string()),
            detail: Some(if record.drafted.is_some() {
                "1 draft".to_string()
            } else {
                format!("{} message(s)", n)
            }),
            mail: Some(serde_json::to_value(record)?),
            ..Default::default()
        },
    )?;
    Ok(e.seq)
}
