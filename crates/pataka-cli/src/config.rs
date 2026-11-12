//! `pataka lists`, `pataka model`, `pataka audit`, `pataka status`.

use std::io::Write;

use pataka_core::error::{Error, Result};
use pataka_core::model::Term;
use pataka_core::model_install::{self, Progress};
use pataka_core::providers::cli as vendor;
use pataka_core::redaction::Category;
use pataka_core::{audit, model::ProviderKind, paths, student};

use crate::ctx::{self, Ctx};

pub enum ListKind {
    Person,
    Place,
    Affiliation,
    Health,
    Tapu,
    Allow,
}

impl ListKind {
    fn category(&self) -> Option<Category> {
        match self {
            ListKind::Person => Some(Category::Person),
            ListKind::Place => Some(Category::Address),
            ListKind::Affiliation => Some(Category::Affiliation),
            ListKind::Health => Some(Category::Health),
            _ => None,
        }
    }
}

pub fn lists_show() -> Result<i32> {
    let ctx = Ctx::open()?;
    let k = ctx.settings()?.kaupapa;
    let show = |title: &str, items: Vec<String>| {
        println!("{title} ({})", items.len());
        for i in items {
            println!("  {i}");
        }
    };
    let by = |c: Category| {
        k.gazetteer
            .iter()
            .filter(|t| t.category == c)
            .map(|t| t.term.clone())
            .collect::<Vec<_>>()
    };
    show("People", by(Category::Person));
    show("Places and addresses", by(Category::Address));
    show("Affiliation (iwi, hapū, marae, whānau)", by(Category::Affiliation));
    show("Health", by(Category::Health));
    show("Tapu terms (block the request)", k.tapu_terms.clone());
    show("Allowed unchanged", k.allow_list.clone());
    Ok(0)
}

pub fn lists_add(kind: ListKind, term: &str) -> Result<i32> {
    let term = term.trim().to_string();
    if term.is_empty() || term.chars().count() > 200 {
        return Err(Error::Invalid("Give a term of 1 to 200 characters.".into()));
    }
    let ctx = Ctx::open()?;
    ctx.update_settings(|s| {
        let k = &mut s.kaupapa;
        match (&kind, kind.category()) {
            (_, Some(category)) => {
                if !k.gazetteer.iter().any(|t| t.term == term && t.category == category) {
                    k.gazetteer.push(Term {
                        term: term.clone(),
                        category,
                    });
                }
            }
            (ListKind::Tapu, _) => {
                if !k.tapu_terms.contains(&term) {
                    k.tapu_terms.push(term.clone());
                }
            }
            _ => {
                if !k.allow_list.contains(&term) {
                    k.allow_list.push(term.clone());
                }
            }
        }
    })?;
    println!("Added “{term}”. It applies in the app and the terminal straight away.");
    Ok(0)
}

pub fn lists_remove(term: &str) -> Result<i32> {
    let ctx = Ctx::open()?;
    let mut removed = 0;
    ctx.update_settings(|s| {
        let k = &mut s.kaupapa;
        let before = k.gazetteer.len() + k.tapu_terms.len() + k.allow_list.len();
        k.gazetteer.retain(|t| t.term != term);
        k.tapu_terms.retain(|t| t != term);
        k.allow_list.retain(|t| t != term);
        removed = before - (k.gazetteer.len() + k.tapu_terms.len() + k.allow_list.len());
    })?;
    if removed == 0 {
        println!("“{term}” was not on any list.");
    } else {
        println!("Removed “{term}”.");
    }
    Ok(0)
}

fn mb(b: u64) -> String {
    format!("{} MB", b / 1_000_000)
}

pub fn model_status() -> Result<i32> {
    let store = ctx::store()?;
    if student::RUNTIME.is_none() {
        println!("ONNX Runtime has no build for this computer, so the name model cannot run here.");
    } else if student::is_installed(store.root()) {
        println!(
            "Installed: {} ({}), ONNX Runtime {}.",
            student::MODEL_NAME,
            student::MODEL_PRECISION,
            student::ORT_VERSION
        );
    } else {
        println!(
            "Not installed. `pataka model install` downloads about {} once.",
            mb(model_install::download_size())
        );
    }
    Ok(0)
}

pub fn model_install() -> Result<i32> {
    let store = ctx::store()?;
    if student::is_installed(store.root()) {
        println!("The name model is already installed.");
        return Ok(0);
    }
    println!(
        "Downloading about {} from huggingface.co (the model) and github.com (ONNX Runtime).",
        mb(model_install::download_size())
    );
    println!("Every file is checked against a SHA-256 built into Pātaka. No text is sent.");
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let progress = |p: Progress| {
        eprint!("\r  {}: {} of {}      ", p.stage, mb(p.done), mb(p.total));
        let _ = std::io::stderr().flush();
    };
    rt.block_on(model_install::install(
        &model_install::download_client(),
        store.root(),
        &progress,
    ))?;
    eprintln!();
    // Load it once, so a broken install shows now.
    student::Student::load(store.root())?;
    audit::append(
        &store.audit_path(),
        audit::Entry {
            event: "model-installed".into(),
            detail: Some(format!(
                "{}@{}, in the terminal",
                student::MODEL_REPO,
                student::MODEL_REVISION
            )),
            ..Default::default()
        },
    )?;
    println!("Installed and checked.");
    Ok(0)
}

pub fn model_remove() -> Result<i32> {
    let store = ctx::store()?;
    model_install::remove(store.root())?;
    println!("Removed. Names will only be caught by your kaitiaki lists.");
    Ok(0)
}

pub fn audit_verify() -> Result<i32> {
    let store = ctx::store()?;
    let v = audit::verify(&store.audit_path());
    if v.ok {
        println!(
            "The audit log is intact: {} entries, each chained to the one before.",
            v.entries
        );
        Ok(0)
    } else {
        println!(
            "The audit log has been changed outside Pātaka: the chain breaks at entry {} of {}.",
            v.first_bad.unwrap_or(0),
            v.entries
        );
        Ok(1)
    }
}

pub fn audit_show(limit: usize) -> Result<i32> {
    let store = ctx::store()?;
    for e in audit::recent(&store.audit_path(), limit)?.into_iter().rev() {
        let found: Vec<String> = e.categories.iter().map(|(k, v)| format!("{v} {k}")).collect();
        println!(
            "{:>5}  {}  {:<18} {}{}{}",
            e.seq,
            e.ts,
            e.event,
            e.provider.as_deref().unwrap_or(""),
            if found.is_empty() {
                String::new()
            } else {
                format!("  [{}]", found.join(", "))
            },
            e.detail.as_deref().map(|d| format!("  {d}")).unwrap_or_default()
        );
    }
    Ok(0)
}

pub fn status() -> Result<i32> {
    let dir = paths::data_dir().ok_or_else(|| Error::Invalid("no data folder".into()))?;
    let store = ctx::store()?;
    println!("Data folder:   {} (shared with the Pātaka app)", dir.display());
    println!(
        "Protection:    {}",
        match store.key_mode()? {
            None => "not set up — run `pataka setup`".to_string(),
            Some(m) => format!("{m:?}").to_lowercase(),
        }
    );
    println!(
        "Name model:    {}",
        if student::is_installed(store.root()) {
            "installed"
        } else if student::RUNTIME.is_none() {
            "not available on this computer"
        } else {
            "not installed — `pataka model install`"
        }
    );
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let st = rt.block_on(vendor::status(ProviderKind::ClaudeCode, None));
    println!(
        "Claude Code:   {}",
        if st.found {
            st.detail.clone()
        } else {
            "not found".into()
        }
    );
    let (prompt, tools) = crate::hooks::installed().unwrap_or((false, false));
    println!(
        "Hooks:         {}",
        match (prompt, tools) {
            (true, true) => "prompt check and tool guard installed",
            (true, false) => "prompt check installed",
            (false, true) => "tool guard installed",
            _ => "not installed — `pataka hooks install`",
        }
    );
    let v = audit::verify(&store.audit_path());
    println!(
        "Audit log:     {} entries, {}",
        v.entries,
        if v.ok { "intact" } else { "CHANGED OUTSIDE PĀTAKA" }
    );
    Ok(0)
}
