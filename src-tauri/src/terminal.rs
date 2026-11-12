//! Settings → Claude Code: install the `pataka` command (bundled with the
//! app) so Claude Code in a terminal can run through Tūmataiti, and add or
//! remove its hooks. Nothing here runs without the person pressing a button.

use std::path::PathBuf;

use serde::Serialize;
use tauri::State;

use crate::audit;
use crate::claude_hooks;
use crate::error::{Error, Result};
use crate::state::AppState;

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "pataka.exe"
    } else {
        "pataka"
    }
}

/// The copy of `pataka` that ships next to the app's own executable, as
/// `pataka-cli` (so it never clashes with a `pataka` already on the PATH).
pub(crate) fn bundled() -> Option<PathBuf> {
    let name = if cfg!(windows) { "pataka-cli.exe" } else { "pataka-cli" };
    let p = std::env::current_exe().ok()?.parent()?.join(name);
    let ok = std::fs::metadata(&p).map(|m| m.len() > 1024).unwrap_or(false);
    ok.then_some(p)
}

/// Where `pataka` is installed for this person.
fn install_dir() -> Result<PathBuf> {
    if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA").ok_or_else(|| Error::Invalid("LOCALAPPDATA is not set.".into()))?;
        Ok(PathBuf::from(base).join("Pataka").join("bin"))
    } else {
        let home = std::env::var_os("HOME").ok_or_else(|| Error::Invalid("HOME is not set.".into()))?;
        Ok(PathBuf::from(home).join(".local").join("bin"))
    }
}

/// Where the installed `pataka` command is (or would be).
pub(crate) fn installed_path() -> Option<PathBuf> {
    install_dir().ok().map(|d| d.join(exe_name()))
}

fn on_path(dir: &std::path::Path) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStatus {
    bundled: bool,
    installed: Option<String>,
    dir: String,
    on_path: bool,
    /// What to add to the shell profile if the folder is not on PATH.
    path_hint: Option<String>,
    prompt_hook: bool,
    tool_hook: bool,
    settings_file: String,
}

fn status_view() -> Result<TerminalStatus> {
    let dir = install_dir()?;
    let target = dir.join(exe_name());
    let (prompt_hook, tool_hook) = claude_hooks::installed().unwrap_or((false, false));
    let on = on_path(&dir) || cfg!(windows) && target.exists();
    Ok(TerminalStatus {
        bundled: bundled().is_some(),
        installed: target.exists().then(|| target.display().to_string()),
        dir: dir.display().to_string(),
        on_path: on,
        path_hint: (!on && !cfg!(windows)).then(|| {
            let shell = if cfg!(target_os = "macos") {
                "~/.zshrc"
            } else {
                "~/.bashrc"
            };
            format!("echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> {shell}")
        }),
        prompt_hook,
        tool_hook,
        settings_file: claude_hooks::settings_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    })
}

#[tauri::command]
pub fn terminal_status(state: State<'_, AppState>) -> Result<TerminalStatus> {
    state.with_session(|_| Ok(()))?;
    status_view()
}

#[cfg(windows)]
fn add_to_user_path(dir: &std::path::Path) -> Result<()> {
    // Read and write the user PATH through .NET, which (unlike setx) does not truncate it.
    let d = dir.display().to_string().replace('\'', "''");
    let script = format!(
        "$d='{d}'; $p=[Environment]::GetEnvironmentVariable('Path','User'); if(-not $p){{$p=''}}; \
         if(($p -split ';') -notcontains $d){{[Environment]::SetEnvironmentVariable('Path', ($p.TrimEnd(';') + ';' + $d).TrimStart(';'), 'User')}}"
    );
    let ok = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid("Could not add Pātaka to your PATH.".into()))
    }
}

/// Copy the bundled `pataka` into the person's own bin folder, and
/// optionally add the hooks to Claude Code.
#[tauri::command]
pub fn terminal_install(state: State<'_, AppState>, hooks: bool, tool_guard: bool) -> Result<TerminalStatus> {
    state.with_session(|_| Ok(()))?;
    let src = bundled().ok_or_else(|| {
        Error::Invalid(
            "This build of Pātaka does not include the pataka command. Install it with the one-line installer in the README instead."
                .into(),
        )
    })?;
    let dir = install_dir()?;
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(exe_name());
    let tmp = dir.join(format!("{}.new", exe_name()));
    std::fs::copy(&src, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &target)?;
    #[cfg(windows)]
    add_to_user_path(&dir)?;
    if hooks {
        claude_hooks::install_for(&target.display().to_string(), tool_guard)?;
    }
    audit::append(
        &state.store.audit_path(),
        audit::Entry {
            event: "terminal-installed".into(),
            detail: Some(format!(
                "{}{}",
                target.display(),
                if hooks { ", hooks added" } else { "" }
            )),
            ..Default::default()
        },
    )?;
    status_view()
}

#[tauri::command]
pub fn terminal_remove_hooks(state: State<'_, AppState>) -> Result<TerminalStatus> {
    state.with_session(|_| Ok(()))?;
    if claude_hooks::remove()? {
        audit::append(
            &state.store.audit_path(),
            audit::Entry {
                event: "hooks-removed".into(),
                ..Default::default()
            },
        )?;
    }
    status_view()
}
