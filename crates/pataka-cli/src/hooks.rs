//! Claude Code hooks.
//!
//! - `UserPromptSubmit`: refuse a prompt that holds tapu terms or secrets.
//!   Under `pataka claude` the running proxy checks it against the kaupapa;
//!   otherwise only the rules for secrets apply.
//! - `PreToolUse` (optional): ask first before commands that send data off
//!   the computer or reach outside the project folder, web fetches and MCP
//!   tools, using the same guards as the app. Pātaka Mail's changes are
//!   checked in every permission mode, including auto.
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

/// Pātaka Mail's answer, in any mode. Changes that need approval ask; in
/// auto mode every change asks, because email is untrusted input that can
/// carry instructions aimed at Claude, and its auto-mode checks read that
/// same input. With prompts switched off (`bypassPermissions`, `dontAsk`)
/// nobody could say yes, so the change is refused.
fn mail_decision(input: &Value) -> Option<Value> {
    let tool = input["tool_name"].as_str().unwrap_or("");
    let reason = guards::review_mail(tool, &input["tool_input"], defers_to_session(input))?;
    let mode = input["permission_mode"].as_str().unwrap_or("");
    let (decision, text) = if matches!(mode, "bypassPermissions" | "dontAsk") {
        (
            "deny",
            format!("Pātaka refused: {reason}, and permission prompts are switched off. Switch to the default or auto mode to approve it."),
        )
    } else {
        ("ask", format!("Pātaka asks because {reason}. Email can contain instructions aimed at Claude, so check this is what you asked for."))
    };
    Some(
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": decision,
                "permissionDecisionReason": text}}),
    )
}

/// `pataka hook pretool`: Claude Code's PreToolUse hook.
pub fn hook_pretool() -> Result<i32> {
    let input = stdin_json();
    let tool = input["tool_name"].as_str().unwrap_or("");
    // Pātaka Mail's tools are judged in every mode, before the guard steps aside.
    if tool.starts_with(pataka_core::mail::TOOL_PREFIX) {
        if let Some(out) = mail_decision(&input) {
            println!("{out}");
        }
        return Ok(0);
    }
    if defers_to_session(&input) {
        return Ok(0);
    }
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

    fn mail(tool: &str, n: usize, mode: &str) -> Option<String> {
        let ids: Vec<String> = (0..n).map(|i| format!("g1.{i}")).collect();
        mail_decision(
            &json!({"tool_name": format!("mcp__pataka-mail__{tool}"), "permission_mode": mode,
                              "tool_input": {"ids": ids}}),
        )
        .map(|v| {
            v["hookSpecificOutput"]["permissionDecision"]
                .as_str()
                .unwrap()
                .to_string()
        })
    }

    #[test]
    fn mail_changes_are_not_left_to_auto_mode() {
        // Reading never asks, in any mode.
        assert_eq!(mail("search", 0, "default"), None);
        assert_eq!(mail("read", 0, "auto"), None);
        // Small changes follow Claude Code's own settings, except in auto mode.
        assert_eq!(mail("label", 3, "default"), None);
        assert_eq!(mail("label", 3, "auto").as_deref(), Some("ask"));
        // Bulk changes and trash always ask.
        assert_eq!(mail("archive", 26, "default").as_deref(), Some("ask"));
        assert_eq!(mail("trash", 1, "acceptEdits").as_deref(), Some("ask"));
        assert_eq!(mail("trash", 1, "auto").as_deref(), Some("ask"));
        // With prompts off, a change that needs a yes is refused.
        assert_eq!(mail("trash", 1, "bypassPermissions").as_deref(), Some("deny"));
        assert_eq!(mail("mark", 2, "dontAsk").as_deref(), Some("deny"));
    }

    #[test]
    fn secrets_are_refused_without_the_proxy() {
        assert!(!local_prompt_reasons("my key is sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEF").is_empty());
        assert!(local_prompt_reasons("please refactor app.py").is_empty());
    }
}
