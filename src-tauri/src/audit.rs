//! Append-only, hash-chained audit log (Tūmataiti SPEC §6.7).
//!
//! Each line records what kind of data left the device, where it went and
//! under which policy: counts per category and action, never the text, never
//! placeholder mappings. Each entry carries the SHA-256 of the previous one,
//! so editing or deleting a past entry breaks the chain and is detected.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub seq: u64,
    pub ts: i64,
    pub event: String,
    #[serde(default)]
    pub korero: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub jurisdiction: Option<String>,
    #[serde(default)]
    pub trust: Option<String>,
    #[serde(default)]
    pub classification: Option<String>,
    #[serde(default)]
    pub policy_hash: Option<String>,
    #[serde(default)]
    pub detector: Option<String>,
    #[serde(default)]
    pub categories: BTreeMap<String, u32>,
    #[serde(default)]
    pub actions: BTreeMap<String, u32>,
    #[serde(default)]
    pub outbound_chars: Option<usize>,
    #[serde(default)]
    pub detail: Option<String>,
    pub prev: String,
    pub hash: String,
}

fn digest(e: &Entry) -> String {
    let mut copy = e.clone();
    copy.hash = String::new();
    let bytes = serde_json::to_vec(&copy).expect("entry serialises");
    hex::encode(Sha256::digest(bytes))
}

fn read_all(path: &Path) -> Result<Vec<Entry>> {
    let f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line?;
        if !line.trim().is_empty() {
            out.push(serde_json::from_str(&line)?);
        }
    }
    Ok(out)
}

pub fn append(path: &Path, mut e: Entry) -> Result<Entry> {
    let last = read_all(path)?.pop();
    e.seq = last.as_ref().map(|l| l.seq + 1).unwrap_or(0);
    e.prev = last.map(|l| l.hash).unwrap_or_else(|| "0".repeat(64));
    e.ts = crate::model::now();
    e.hash = digest(&e);
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    crate::store::restrict(path);
    writeln!(f, "{}", serde_json::to_string(&e)?)?;
    f.sync_all()?;
    Ok(e)
}

pub fn recent(path: &Path, limit: usize) -> Result<Vec<Entry>> {
    let all = read_all(path)?;
    let skip = all.len().saturating_sub(limit);
    Ok(all.into_iter().skip(skip).rev().collect())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verification {
    pub ok: bool,
    pub entries: usize,
    pub first_bad: Option<u64>,
}

pub fn verify(path: &Path) -> Verification {
    let all = match read_all(path) {
        Ok(a) => a,
        Err(_) => {
            return Verification {
                ok: false,
                entries: 0,
                first_bad: Some(0),
            }
        }
    };
    let mut prev = "0".repeat(64);
    for (i, e) in all.iter().enumerate() {
        if e.prev != prev || digest(e) != e.hash || e.seq != i as u64 {
            return Verification {
                ok: false,
                entries: all.len(),
                first_bad: Some(i as u64),
            };
        }
        prev = e.hash.clone();
    }
    Verification {
        ok: true,
        entries: all.len(),
        first_bad: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_detects_edits() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        for i in 0..3 {
            append(
                &p,
                Entry {
                    event: format!("send{i}"),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        assert!(verify(&p).ok);
        let text = fs::read_to_string(&p).unwrap().replace("send1", "send9");
        fs::write(&p, text).unwrap();
        let v = verify(&p);
        assert!(!v.ok);
        assert_eq!(v.first_bad, Some(1));
    }
}
