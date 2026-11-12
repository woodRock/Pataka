//! Pātaka's Claude Code hooks in `~/.claude/settings.json`: merging them in
//! next to the person's own settings, finding them, and taking them out.
//! Shared by the `pataka` command and the app's Settings.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::error::{Error, Result};

pub const TOOL_MATCHER: &str = "Bash|BashOutput|Read|Write|Edit|MultiEdit|NotebookEdit|Glob|Grep|LS|WebFetch|mcp__.*";

pub fn settings_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("settings.json"));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .ok_or_else(|| Error::Invalid("Could not find your home folder.".into()))?;
    Ok(PathBuf::from(home).join(".claude").join("settings.json"))
}

pub fn exe() -> Result<String> {
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

pub fn read_settings(path: &Path) -> Result<Value> {
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

pub fn write_settings(path: &Path, value: &Value) -> Result<()> {
    if path.exists() {
        let backup = path.with_extension(format!("json.pataka-backup-{}", crate::model::now()));
        std::fs::copy(path, &backup)?;
        eprintln!("Saved a copy of the old file as {}", backup.display());
    }
    let text = serde_json::to_string_pretty(value)? + "\n";
    crate::store::write_atomic(path, text.as_bytes())
}

pub fn installed() -> Result<(bool, bool)> {
    let s = read_settings(&settings_path()?)?;
    let has = |event: &str| s["hooks"][event].as_array().is_some_and(|a| a.iter().any(is_ours));
    Ok((has("UserPromptSubmit"), has("PreToolUse")))
}

/// Add the hooks for `exe` (an absolute path to `pataka`), keeping a backup.
pub fn install_for(exe: &str, guard_tools: bool) -> Result<PathBuf> {
    let path = settings_path()?;
    let settings = merged(read_settings(&path)?, exe, guard_tools, true);
    write_settings(&path, &settings)?;
    Ok(path)
}

/// Remove Pātaka's hooks. Returns false if there were none.
pub fn remove() -> Result<bool> {
    let path = settings_path()?;
    let before = read_settings(&path)?;
    let after = merged(before.clone(), "", false, false);
    if after == before {
        return Ok(false);
    }
    write_settings(&path, &after)?;
    Ok(true)
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
}
