//! Pātaka Calendar: Claude Code reads the user's Google Calendar and adds
//! events to it, through an MCP server inside the proxy, the same way as
//! Pātaka's mail server.
//!
//! - The account (the OAuth client the person made in Google Cloud, and the
//!   grant from signing in) is stored encrypted in `calendar.ptk` under
//!   Pātaka's data key. Pātaka talks only to Google's OAuth and Calendar API
//!   hosts, and to nothing else.
//! - Tool calls arrive with real values: the proxy restores placeholders in
//!   the arguments of `mcp__pataka-calendar__*` tools only, for sessions that
//!   registered the server. Results are redacted before Claude Code sees
//!   them.
//! - Nothing is sent to anyone: events are created without attendees and
//!   with `sendUpdates=none`, so Google emails nobody.
//! - Pātaka only changes or deletes events it added itself (they carry a
//!   private `pataka` property), never ones from invitations or the person.
//! - Every change is written to the audit log with event ids and hashed
//!   calendar references, never titles, places or times, and can be undone
//!   with `pataka calendar undo <entry>`. What undo needs to put an edited or
//!   deleted event back is kept encrypted in `calendar.ptk`.

pub mod google;
pub mod mcp;
pub mod oauth;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::crypto::DataKey;
use crate::error::{Error, Result};
use crate::mail::NameRef;
use crate::store::Store;

/// Name prefix of this server's tools as Claude Code sees them.
pub const TOOL_PREFIX: &str = "mcp__pataka-calendar__";
/// The server name given to Claude Code.
pub const SERVER_NAME: &str = "pataka-calendar";
/// Where the server answers, under a session's base address.
pub const PATH: &str = "pataka/calendar";

/// A string kept out of logs and wiped from memory when dropped.
pub type Hidden = Zeroizing<String>;

/// `https://<parts joined by dots><path>`, for Google's fixed hosts.
fn google(parts: &[&str], path: &str) -> String {
    format!("https://{}{path}", parts.join("."))
}

/// The Calendar API, v3.
pub fn google_api() -> String {
    google(&["www", "googleapis", "com"], "/calendar/v3")
}

/// Where codes and grants are exchanged for access.
pub fn google_token() -> String {
    google(&["oauth2", "googleapis", "com"], "/token")
}

/// Where the person signs in, in their own browser.
pub fn google_auth() -> String {
    google(&["accounts", "google", "com"], "/o/oauth2/v2/auth")
}

/// Where a grant is revoked on disconnect.
pub fn google_revoke() -> String {
    google(&["oauth2", "googleapis", "com"], "/revoke")
}

/// Events (read and write) and the list of calendars (read only). Not the
/// full `calendar` scope: Pātaka cannot change calendars' settings or sharing.
pub fn scopes() -> String {
    let base = google(&["www", "googleapis", "com"], "/auth/calendar");
    format!("{base}.events {base}.calendarlist.readonly")
}

/// A connected Google account. Not `Debug`, so its keys cannot be logged.
#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    /// The primary calendar's id: the account's address.
    pub email: String,
    /// The person's own OAuth client ("Desktop app") from Google Cloud.
    pub client_id: String,
    pub client_code: Hidden,
    /// The long-lived grant from signing in.
    pub grant: Hidden,
    /// Always Google's, except in tests against a server on this computer.
    #[serde(default = "google_api")]
    pub api: String,
    #[serde(default = "google_token")]
    pub token_url: String,
}

impl Account {
    /// The host the audit log names for this account's changes.
    pub fn host(&self) -> String {
        url::Url::parse(&self.api)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_default()
    }
}

/// `calendar.ptk`: the account, the calendar-name table for the audit log's
/// hashed references, and the snapshots undo puts back.
#[derive(Default, Serialize, Deserialize)]
pub struct CalendarFile {
    #[serde(default)]
    pub account: Option<Account>,
    #[serde(default)]
    pub salt: String,
    /// Reference → calendar id.
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    /// Reference → an event's fields before Pātaka changed or deleted it.
    #[serde(default)]
    pub snapshots: BTreeMap<String, serde_json::Value>,
}

/// Access to `calendar.ptk` with the unlocked data key.
#[derive(Clone)]
pub struct CalendarVault {
    pub store: Store,
    pub key: DataKey,
}

impl CalendarVault {
    pub fn load(&self) -> Result<CalendarFile> {
        self.store.load_calendar(&self.key)
    }

    pub fn save(&self, f: &CalendarFile) -> Result<()> {
        self.store.save_calendar(&self.key, f)
    }

    pub fn account(&self) -> Result<Account> {
        self.load()?.account.ok_or_else(|| {
            Error::Invalid("No calendar is connected. Run `pataka calendar connect` in a terminal first.".into())
        })
    }

    pub fn connected(&self) -> Result<bool> {
        Ok(self.load()?.account.is_some())
    }

    pub fn set_account(&self, account: Option<Account>) -> Result<()> {
        let mut f = self.load()?;
        if account.is_none() {
            // Nothing of the old account's calendars stays behind.
            f.names.clear();
            f.snapshots.clear();
        }
        f.account = account;
        self.save(&f)
    }

    /// A hashed reference for a calendar id, remembering the id.
    pub fn name_ref(&self, calendar: &str) -> Result<NameRef> {
        let mut f = self.load()?;
        let mut changed = false;
        if f.salt.is_empty() {
            f.salt = crate::agent_proxy::new_token();
            changed = true;
        }
        let hash = name_hash(&f.salt, calendar);
        let reference = format!("calendar.ptk#{hash}");
        if !f.names.contains_key(&reference) {
            f.names.insert(reference.clone(), calendar.to_string());
            changed = true;
        }
        if changed {
            self.save(&f)?;
        }
        Ok(NameRef { hash, reference })
    }

    /// The calendar id behind a reference, checked against its hash.
    pub fn resolve(&self, r: &NameRef) -> Result<String> {
        let f = self.load()?;
        let name = f
            .names
            .get(&r.reference)
            .ok_or_else(|| Error::NotFound("that calendar in Pātaka's calendar store".into()))?;
        if name_hash(&f.salt, name) != r.hash {
            return Err(Error::Integrity);
        }
        Ok(name.clone())
    }

    /// Keep an event's fields for undo. Returns the reference to log.
    pub fn snapshot(&self, event: &serde_json::Value) -> Result<String> {
        let mut f = self.load()?;
        let reference = format!("calendar.ptk#snap-{}", crate::agent_proxy::new_token());
        f.snapshots.insert(reference.clone(), event.clone());
        self.save(&f)?;
        Ok(reference)
    }

    pub fn snapshot_of(&self, reference: &str) -> Result<serde_json::Value> {
        self.load()?
            .snapshots
            .get(reference)
            .cloned()
            .ok_or_else(|| Error::NotFound("what that event was before (the calendar was disconnected since)".into()))
    }
}

fn name_hash(salt: &str, name: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"pataka-calendar-name\0");
    h.update(salt.as_bytes());
    h.update([0u8]);
    h.update(name.as_bytes());
    hex::encode(&h.finalize()[..12])
}

/// What one change did: enough to undo it, and nothing about the event's
/// content.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Record {
    /// `create`, `update`, `delete`, or `undo`.
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar: Option<NameRef>,
    /// The Google event id.
    pub event: String,
    /// For `update` and `delete`: the encrypted snapshot of what was there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// For an undo that put a deleted event back: the new event's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored: Option<String>,
    /// For an undo: the entry it reversed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undoes: Option<u64>,
}

/// Write a change to the audit log. Returns the entry's number.
pub fn audit_record(audit_path: &std::path::Path, korero: Option<&str>, host: &str, record: &Record) -> Result<u64> {
    let e = crate::audit::append(
        audit_path,
        crate::audit::Entry {
            event: format!("calendar-{}", record.action),
            korero: korero.map(String::from),
            provider: Some(host.to_string()),
            detail: Some("1 event".into()),
            calendar: Some(serde_json::to_value(record)?),
            ..Default::default()
        },
    )?;
    Ok(e.seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn vault() -> (tempfile::TempDir, CalendarVault) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf()).unwrap();
        (
            dir,
            CalendarVault {
                store,
                key: DataKey::generate(),
            },
        )
    }

    #[test]
    fn calendar_refs_resolve_and_detect_tampering() {
        let (_d, v) = vault();
        let r = v.name_ref("primary").unwrap();
        assert_eq!(v.name_ref("primary").unwrap(), r);
        assert_eq!(v.resolve(&r).unwrap(), "primary");
        let bad = NameRef {
            hash: "00".into(),
            reference: r.reference.clone(),
        };
        assert!(matches!(v.resolve(&bad), Err(Error::Integrity)));
    }

    #[test]
    fn disconnecting_forgets_snapshots() {
        let (_d, v) = vault();
        let s = v.snapshot(&serde_json::json!({"summary": "x"})).unwrap();
        assert!(v.snapshot_of(&s).is_ok());
        v.set_account(None).unwrap();
        assert!(v.snapshot_of(&s).is_err());
    }

    #[test]
    fn google_hosts_are_googles() {
        for u in [google_api(), google_token(), google_auth(), google_revoke()] {
            let host = url::Url::parse(&u).unwrap().host_str().unwrap().to_string();
            assert!(
                host.ends_with(".googleapis.com") || host == "accounts.google.com",
                "{host}"
            );
        }
        assert!(scopes().ends_with("calendar.calendarlist.readonly"));
    }
}
