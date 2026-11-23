//! Append-only, hash-chained audit log (Tūmataiti SPEC §6.7).
//!
//! Each line records what kind of data left the device, where it went and
//! under which policy: counts per category and action, never the text, never
//! placeholder mappings. Each entry carries the SHA-256 of the previous one,
//! so editing or deleting a past entry breaks the chain and is detected.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

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
    /// Pātaka Mail: what a mutation changed (ids, hashed label references),
    /// so it can be undone. Never subjects, addresses or message text.
    /// Omitted when empty, so older entries hash exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail: Option<serde_json::Value>,
    /// Pātaka Calendar: what a change did (event ids, hashed calendar
    /// references, a reference to an encrypted snapshot), so it can be
    /// undone. Never titles, places, times or attendees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar: Option<serde_json::Value>,
    /// `audit-repaired` entries: the forks they acknowledge (see [`repair`]).
    /// Omitted when empty, so older entries hash exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<serde_json::Value>,
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

/// Opens the log for appending and holds an exclusive lock on it until the
/// returned file is dropped. The service, the CLI and the desktop app all
/// append to the same file, so the tail must be read and written under one
/// lock; otherwise two writers can read the same last entry and fork the chain.
fn lock(path: &Path) -> Result<fs::File> {
    let f = OpenOptions::new().create(true).append(true).open(path)?;
    crate::store::restrict(path);
    f.lock()?;
    Ok(f)
}

fn write_locked(f: &mut fs::File, path: &Path, mut e: Entry) -> Result<Entry> {
    let last = read_all(path)?.pop();
    e.seq = last.as_ref().map(|l| l.seq + 1).unwrap_or(0);
    e.prev = last.map(|l| l.hash).unwrap_or_else(|| GENESIS.to_string());
    e.ts = crate::model::now();
    e.hash = digest(&e);
    // One write call per line, so a reader never sees half an entry.
    f.write_all(format!("{}\n", serde_json::to_string(&e)?).as_bytes())?;
    f.sync_all()?;
    Ok(e)
}

pub fn append(path: &Path, e: Entry) -> Result<Entry> {
    let mut f = lock(path)?;
    write_locked(&mut f, path, e)
}

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Every entry, oldest first.
pub fn all(path: &Path) -> Result<Vec<Entry>> {
    read_all(path)
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
    /// Line (0-based) of the first entry that fails the check.
    pub first_bad: Option<u64>,
    /// Forks from concurrent writes that an `audit-repaired` entry
    /// acknowledges (see [`repair`]). The chain is intact around them.
    pub acknowledged: usize,
}

/// A place where two writers appended at once: `line` links to an earlier
/// entry rather than to the line before it, as the line before it does too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fork {
    /// 0-based line of the entry that links past its neighbour.
    pub line: u64,
    pub seq: u64,
    pub hash: String,
    /// Hash of the entry it links to (the one both writers read as last).
    pub links_to: String,
    /// Line and hash of the sibling written at the same moment, which the
    /// chain no longer passes through. It stays in the log, unchanged.
    pub sibling_line: u64,
    pub sibling_hash: String,
}

/// Forks acknowledged by `audit-repaired` entries, by the hash of the
/// entry that links past its sibling.
fn acknowledged(all: &[Entry]) -> BTreeMap<String, (usize, Fork)> {
    let mut out = BTreeMap::new();
    for (i, e) in all.iter().enumerate() {
        if e.event != REPAIRED {
            continue;
        }
        let forks = e
            .repair
            .as_ref()
            .and_then(|r| r.get("forks"))
            .and_then(|f| serde_json::from_value::<Vec<Fork>>(f.clone()).ok())
            .unwrap_or_default();
        for f in forks {
            out.entry(f.hash.clone()).or_insert((i, f));
        }
    }
    out
}

const REPAIRED: &str = "audit-repaired";

/// Walks the chain. `acks` lets a link break through only where it is
/// exactly an acknowledged fork recorded later in the log; an entry whose
/// own hash does not match its content never passes.
fn walk(all: &[Entry], acks: &BTreeMap<String, (usize, Fork)>) -> (Option<usize>, usize) {
    let mut prev = GENESIS.to_string();
    let mut next_seq = 0u64;
    let mut used = 0;
    for (i, e) in all.iter().enumerate() {
        if digest(e) != e.hash {
            return (Some(i), used);
        }
        if e.prev != prev || e.seq != next_seq {
            let ok = acks.get(&e.hash).is_some_and(|(at, f)| {
                *at > i
                    && f.line == i as u64
                    && f.seq == e.seq
                    && f.links_to == e.prev
                    && i >= 1
                    && f.sibling_line == (i - 1) as u64
                    && f.sibling_hash == all[i - 1].hash
                    && all[i - 1].prev == e.prev
                    && all[i - 1].seq == e.seq
            });
            if !ok {
                return (Some(i), used);
            }
            used += 1;
        }
        prev = e.hash.clone();
        next_seq = e.seq + 1;
    }
    (None, used)
}

pub fn verify(path: &Path) -> Verification {
    let all = match read_all(path) {
        Ok(a) => a,
        Err(_) => {
            return Verification {
                ok: false,
                entries: 0,
                first_bad: Some(0),
                acknowledged: 0,
            }
        }
    };
    let (bad, used) = walk(&all, &acknowledged(&all));
    Verification {
        ok: bad.is_none(),
        entries: all.len(),
        first_bad: bad.map(|b| b as u64),
        acknowledged: used,
    }
}

/// What `repair` found, or did.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairPlan {
    pub entries: usize,
    /// Forks from concurrent writes that can be acknowledged.
    pub forks: Vec<Fork>,
    /// A break that is not such a fork (an edited, removed or reordered
    /// entry). Pātaka will not acknowledge anything while one exists.
    pub unrepairable: Option<u64>,
    /// Where the original log was copied, when applied.
    pub backup: Option<PathBuf>,
    /// The `audit-repaired` entry, when applied.
    pub entry: Option<u64>,
}

/// Finds the forks two concurrent writers left in the chain and, with
/// `apply`, acknowledges them: copies the log to a backup beside it, then
/// appends one `audit-repaired` entry naming each fork. Nothing already in
/// the log is changed, renumbered or removed, so every seq number shown
/// before (and every `undoes` reference) still means the same entry.
///
/// Only a fork is acknowledged: an entry that links to the entry two lines
/// up, with the same seq as the entry between, which links there too, and
/// whose own hash matches its content. Any other break means the log was
/// changed, and `repair` leaves it alone.
pub fn repair(path: &Path, apply: bool) -> Result<RepairPlan> {
    let mut f = lock(path)?;
    let all = read_all(path)?;
    let mut acks = acknowledged(&all);
    let mut forks = Vec::new();
    // Each pass either finishes, finds a fork to acknowledge, or stops.
    let unrepairable = loop {
        let (bad, _) = walk(&all, &acks);
        let Some(i) = bad else { break None };
        let e = &all[i];
        let fork = (i >= 2 && digest(e) == e.hash).then(|| {
            let sib = &all[i - 1];
            let base = &all[i - 2];
            (e.prev == base.hash && sib.prev == base.hash && sib.seq == e.seq).then(|| Fork {
                line: i as u64,
                seq: e.seq,
                hash: e.hash.clone(),
                links_to: e.prev.clone(),
                sibling_line: (i - 1) as u64,
                sibling_hash: sib.hash.clone(),
            })
        });
        match fork.flatten() {
            Some(fk) => {
                acks.insert(fk.hash.clone(), (all.len(), fk.clone()));
                forks.push(fk);
            }
            None => break Some(i as u64),
        }
    };
    let mut plan = RepairPlan {
        entries: all.len(),
        forks,
        unrepairable,
        backup: None,
        entry: None,
    };
    if !apply || plan.forks.is_empty() || plan.unrepairable.is_some() {
        return Ok(plan);
    }
    let backup = path.with_extension(format!("jsonl.before-repair-{}", crate::model::now()));
    fs::copy(path, &backup)?;
    crate::store::restrict(&backup);
    let lines: Vec<String> = plan.forks.iter().map(|f| f.line.to_string()).collect();
    let e = write_locked(
        &mut f,
        path,
        Entry {
            event: REPAIRED.into(),
            detail: Some(format!(
                "acknowledged {} fork(s) from concurrent writes at line(s) {}; no entry changed; original kept at {}",
                plan.forks.len(),
                lines.join(", "),
                backup.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
            )),
            repair: Some(serde_json::json!({ "forks": plan.forks })),
            ..Default::default()
        },
    )?;
    plan.backup = Some(backup);
    plan.entry = Some(e.seq);
    Ok(plan)
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

    fn entry(event: &str) -> Entry {
        Entry {
            event: event.into(),
            ..Default::default()
        }
    }

    #[test]
    fn concurrent_appends_keep_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        let threads = 8;
        let each = 25;
        let start = std::sync::Arc::new(std::sync::Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let p = p.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    for i in 0..each {
                        append(&p, entry(&format!("t{t}-{i}"))).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let v = verify(&p);
        assert!(v.ok, "{v:?}");
        assert_eq!(v.entries, threads * each);
        assert_eq!(v.acknowledged, 0);
    }

    /// Writes what two unlocked writers used to leave: `n` entries, then two
    /// siblings that both link to the last of them, then more after the second.
    fn forked_log(p: &Path, n: usize, after: usize) {
        for i in 0..n {
            append(p, entry(&format!("e{i}"))).unwrap();
        }
        let base = all(p).unwrap().pop().unwrap();
        let mut sibling = entry("sibling");
        sibling.seq = base.seq + 1;
        sibling.prev = base.hash.clone();
        sibling.hash = digest(&sibling);
        let mut line = serde_json::to_string(&sibling).unwrap();
        line.push('\n');
        let mut f = OpenOptions::new().append(true).open(p).unwrap();
        f.write_all(line.as_bytes()).unwrap();
        // The second writer had read the same last entry.
        let mut second = entry("second");
        second.seq = base.seq + 1;
        second.prev = base.hash;
        second.hash = digest(&second);
        f.write_all(format!("{}\n", serde_json::to_string(&second).unwrap()).as_bytes())
            .unwrap();
        drop(f);
        for i in 0..after {
            append(p, entry(&format!("after{i}"))).unwrap();
        }
    }

    #[test]
    fn repair_acknowledges_a_fork_without_changing_entries() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        forked_log(&p, 3, 4);
        let v = verify(&p);
        assert!(!v.ok);
        assert_eq!(v.first_bad, Some(4));
        let before = fs::read_to_string(&p).unwrap();

        let plan = repair(&p, false).unwrap();
        assert_eq!(plan.forks.len(), 1);
        assert_eq!(plan.forks[0].line, 4);
        assert_eq!(plan.forks[0].sibling_line, 3);
        assert_eq!(plan.unrepairable, None);
        assert_eq!(fs::read_to_string(&p).unwrap(), before, "a dry run writes nothing");

        let done = repair(&p, true).unwrap();
        assert_eq!(done.entry, Some(8));
        assert_eq!(fs::read_to_string(done.backup.unwrap()).unwrap(), before);
        let after = fs::read_to_string(&p).unwrap();
        assert!(after.starts_with(&before), "every earlier line is kept as it was");
        let v = verify(&p);
        assert!(v.ok, "{v:?}");
        assert_eq!(v.acknowledged, 1);

        // Nothing left to do, and later appends keep verifying.
        assert!(repair(&p, false).unwrap().forks.is_empty());
        append(&p, entry("later")).unwrap();
        assert!(verify(&p).ok);
    }

    #[test]
    fn repair_refuses_edits_and_acknowledgements_do_not_spread() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        forked_log(&p, 3, 4);
        repair(&p, true).unwrap();
        // An edit after the acknowledged fork is still caught, and repair
        // will not acknowledge it.
        let text = fs::read_to_string(&p).unwrap().replace("after2", "after9");
        fs::write(&p, &text).unwrap();
        assert_eq!(verify(&p).first_bad, Some(7));
        let plan = repair(&p, true).unwrap();
        assert_eq!(plan.unrepairable, Some(7));
        assert!(plan.entry.is_none() && plan.backup.is_none());
        assert_eq!(fs::read_to_string(&p).unwrap(), text);

        // Removing the acknowledgement breaks the chain again.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        forked_log(&p, 2, 1);
        repair(&p, true).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        fs::write(&p, lines[..lines.len() - 1].join("\n") + "\n").unwrap();
        assert!(!verify(&p).ok);

        // A deleted entry is not a fork.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        for i in 0..4 {
            append(&p, entry(&format!("e{i}"))).unwrap();
        }
        let text = fs::read_to_string(&p).unwrap();
        let kept: Vec<&str> = text
            .lines()
            .enumerate()
            .filter(|(i, _)| *i != 2)
            .map(|(_, l)| l)
            .collect();
        fs::write(&p, kept.join("\n") + "\n").unwrap();
        assert_eq!(repair(&p, true).unwrap().unrepairable, Some(2));
    }
}
