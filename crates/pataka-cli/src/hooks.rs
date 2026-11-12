//! Claude Code hooks.
//!
//! - `UserPromptSubmit`: refuse a prompt that holds tapu terms or secrets.
//!   Under `pataka claude` the running proxy checks it against the kaupapa;
//!   otherwise only the rules for secrets apply.
//! - `PreToolUse` (optional): ask first before commands that send data off
//!   the computer or reach outside the project folder, web fetches and MCP
//!   tools, using the same guards as the app.
//!
//! Installed into `~/.claude/settings.json` only with the person's consent;
//! a backup of the file is kept, and other hooks are left alone.

use std::io::Read;
use std::path::{Path, PathBuf};

use pataka_core::error::{Error, Result};
use pataka_core::guards;
use pataka_core::model::Kaupapa;
use pataka_core::redaction::{self, Vault};
use serde_json::{json, Value};

const TOOL_MATCHER: &str = "Bash|BashOutput|Read|Write|Edit|MultiEdit|NotebookEdit|Glob|Grep|LS|WebFetch|mcp__.*";

pub fn settings_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("settings.json"));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .ok_or_else(|| Error::Invalid("Could not find your home folder.".into()))?;
    Ok(PathBuf::from(home).join(".claude").join("settings.json"))
}

fn exe() -> Result<String> {
    let p = std::env::current_exe()?;
    let p = p.canonicalize().unwrap_or(p);
    Ok(p.display().to_string())
}

fn command(exe: &str, which: &str) -> String {
    format!("\"{exe}\" hook {which}")
}

fn is_ours(entry: &Value) -> bool {
    entry["hooks"].as_array().is_some_and(|hs| {
        hs.iter().any(|h| {
            h["command"]
                .as_str()
                .is_some_and(|c| c.contains("pataka") && c.contains(" hook "))
        })
    })
}

/// The settings with Pātaka's hooks removed, then (if `install`) added back.
pub fn merged(mut settings: Value, exe: &str, guard_tools: bool, install: bool) -> Value {
    if !settings.is_object() {
        settings = json!({});
    }
    let hooks = settings
        .as_object_mut()
        .expect("object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().expect("object");
    for event in ["UserPromptSubmit", "PreToolUse"] {
        if let Some(Value::Array(list)) = hooks.get_mut(event) {
            list.retain(|e| !is_ours(e));
        }
    }
    if install {
        if let Some(a) = hooks
            .entry("UserPromptSubmit")
            .or_insert_with(|| json!([]))
            .as_array_mut()
        {
            a.push(json!({"hooks": [{"type": "command", "command": command(exe, "prompt")}]}));
        }
        if guard_tools {
            if let Some(a) = hooks.entry("PreToolUse").or_insert_with(|| json!([])).as_array_mut() {
                a.push(json!({"matcher": TOOL_MATCHER, "hooks": [{"type": "command", "command": command(exe, "pretool")}]}));
            }
        }
    }
    for event in ["UserPromptSubmit", "PreToolUse"] {
        if hooks.get(event).and_then(Value::as_array).is_some_and(|a| a.is_empty()) {
            hooks.remove(event);
        }
    }
    if hooks.is_empty() {
        settings.as_object_mut().expect("object").remove("hooks");
    }
    settings
}

fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(t) if !t.trim().is_empty() => serde_json::from_str(&t).map_err(|_| {
            Error::Invalid(format!(
                "{} is not valid JSON, so Pātaka will not change it.",
                path.display()
            ))
        }),
        Ok(_) => Ok(json!({})),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e.into()),
    }
}

fn write_settings(path: &Path, value: &Value) -> Result<()> {
    if path.exists() {
        let backup = path.with_extension(format!("json.pataka-backup-{}", pataka_core::model::now()));
        std::fs::copy(path, &backup)?;
        eprintln!("Saved a copy of the old file as {}", backup.display());
    }
    let text = serde_json::to_string_pretty(value)? + "\n";
    pataka_core::store::write_atomic(path, text.as_bytes())
}

pub fn installed() -> Result<(bool, bool)> {
    let s = read_settings(&settings_path()?)?;
    let has = |event: &str| s["hooks"][event].as_array().is_some_and(|a| a.iter().any(is_ours));
    Ok((has("UserPromptSubmit"), has("PreToolUse")))
}

pub fn install(guard_tools: bool, yes: bool) -> Result<()> {
    let path = settings_path()?;
    let exe = exe()?;
    println!("Pātaka will add these hooks to {}:", path.display());
    println!("  - before a prompt is sent: refuse it if it contains tapu terms or secrets");
    if guard_tools {
        println!("  - before a tool runs: ask first for commands that send data out (curl, scp, git push, …),");
        println!("    paths outside the project folder, web fetches and MCP tools");
    }
    println!("They run {exe}. Your other settings and hooks are kept, and a backup is saved.");
    if !yes && !crate::ctx::confirm("Add them?", false) {
        println!("Nothing changed.");
        return Ok(());
    }
    let settings = merged(read_settings(&path)?, &exe, guard_tools, true);
    write_settings(&path, &settings)?;
    println!("Hooks added. Remove them any time with `pataka hooks uninstall`.");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let path = settings_path()?;
    let before = read_settings(&path)?;
    let after = merged(before.clone(), "", false, false);
    if after == before {
        println!("No Pātaka hooks were installed in {}.", path.display());
        return Ok(());
    }
    write_settings(&path, &after)?;
    println!("Pātaka's hooks were removed from {}.", path.display());
    Ok(())
}

fn stdin_json() -> Value {
    let mut s = String::new();
    let _ = std::io::stdin().take(8 * 1024 * 1024).read_to_string(&mut s);
    serde_json::from_str(&s).unwrap_or(Value::Null)
}

/// Rules only (no kaupapa): used when Claude Code runs without `pataka claude`.
fn local_prompt_reasons(prompt: &str) -> Vec<String> {
    let mut vault = Vault::default();
    let out = redaction::redact(prompt, &Kaupapa::default(), &[], &[], &[], &mut vault);
    let mut r: Vec<String> = out
        .findings
        .iter()
        .filter(|f| f.action == redaction::Action::Block)
        .map(|_| "it contains what looks like a password or API key".to_string())
        .collect();
    r.dedup();
    r
}

/// `pataka hook prompt`: Claude Code's UserPromptSubmit hook.
pub fn hook_prompt() -> Result<i32> {
    let input = stdin_json();
    let prompt = input["prompt"].as_str().unwrap_or("");
    let reasons: Vec<String> = match std::env::var("PATAKA_HOOK_URL") {
        Ok(url) if url.starts_with("http://127.0.0.1:") => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            let res: std::result::Result<Value, reqwest::Error> = rt.block_on(async {
                reqwest::Client::new()
                    .post(&url)
                    .json(&json!({"prompt": prompt}))
                    .send()
                    .await?
                    .json()
                    .await
            });
            match res {
                Ok(v) => v["reasons"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
                // The proxy is gone: refuse rather than let the prompt through unchecked.
                Err(_) => vec!["Pātaka's proxy is not running, so it could not check this prompt".into()],
            }
        }
        _ => local_prompt_reasons(prompt),
    };
    if !reasons.is_empty() {
        println!(
            "{}",
            json!({"decision": "block", "reason": format!("Pātaka stopped this prompt: {}. Nothing was sent.", reasons.join("; "))})
        );
    }
    Ok(0)
}

/// `pataka hook pretool`: Claude Code's PreToolUse hook.
pub fn hook_pretool() -> Result<i32> {
    let input = stdin_json();
    let tool = input["tool_name"].as_str().unwrap_or("");
    let cwd = input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    if let Some(reason) = guards::review_tool(tool, &input["tool_input"], &cwd) {
        println!(
            "{}",
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "ask",
                   "permissionDecisionReason": format!("Pātaka asks because {reason}.")}})
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_keeps_other_hooks_and_is_idempotent() {
        let mine = json!({"model": "opus", "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "my-lint"}]}]}});
        let once = merged(mine.clone(), "/usr/bin/pataka", true, true);
        let twice = merged(once.clone(), "/usr/bin/pataka", true, true);
        assert_eq!(once, twice);
        assert_eq!(once["model"], "opus");
        assert_eq!(once["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert!(once["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with("hook prompt"));
        let removed = merged(once, "", false, false);
        assert_eq!(removed, mine);
    }

    #[test]
    fn secrets_are_refused_without_the_proxy() {
        assert!(!local_prompt_reasons("my key is sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEF").is_empty());
        assert!(local_prompt_reasons("please refactor app.py").is_empty());
    }
}
