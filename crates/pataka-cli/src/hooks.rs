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
use std::path::PathBuf;

use pataka_core::claude_hooks::{self, merged, read_settings, settings_path, write_settings};
use pataka_core::error::Result;
use pataka_core::guards;
use pataka_core::model::Kaupapa;
use pataka_core::redaction::{self, Vault};
use serde_json::{json, Value};

pub fn install(guard_tools: bool, yes: bool) -> Result<()> {
    let path = settings_path()?;
    let exe = claude_hooks::exe()?;
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
    if claude_hooks::remove()? {
        println!("Pātaka's hooks were removed from {}.", path.display());
    } else {
        println!("No Pātaka hooks were installed in {}.", path.display());
    }
    Ok(())
}

pub fn installed() -> Result<(bool, bool)> {
    claude_hooks::installed()
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

/// Permission modes in which the person has handed approvals to Claude Code
/// (its auto-mode classifier, or no prompts at all). A hook "ask" would
/// override that choice and prompt on every MCP tool, so the guard steps
/// aside. Redaction is unaffected: it happens in the proxy, not here.
const DEFERRING_MODES: [&str; 3] = ["auto", "bypassPermissions", "dontAsk"];

fn defers_to_session(input: &Value) -> bool {
    input["permission_mode"]
        .as_str()
        .is_some_and(|m| DEFERRING_MODES.contains(&m))
}

/// `pataka hook pretool`: Claude Code's PreToolUse hook.
pub fn hook_pretool() -> Result<i32> {
    let input = stdin_json();
    if defers_to_session(&input) {
        return Ok(0);
    }
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
    fn tool_guard_steps_aside_in_auto_mode() {
        assert!(defers_to_session(&json!({"permission_mode": "auto"})));
        assert!(defers_to_session(&json!({"permission_mode": "bypassPermissions"})));
        assert!(!defers_to_session(&json!({"permission_mode": "default"})));
        assert!(!defers_to_session(&json!({"permission_mode": "acceptEdits"})));
        assert!(!defers_to_session(&json!({})));
    }

    #[test]
    fn secrets_are_refused_without_the_proxy() {
        assert!(!local_prompt_reasons("my key is sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEF").is_empty());
        assert!(local_prompt_reasons("please refactor app.py").is_empty());
    }
}
