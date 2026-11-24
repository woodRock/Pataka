//! Settings → Calendar: connect Google Calendar for Pātaka Calendar from the
//! app, the same account `pataka calendar connect` sets up in a terminal.
//! The person signs in with their own OAuth client in their browser; the
//! grant is kept encrypted in Pātaka's store.

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::calendar::{self, CalendarVault};
use crate::claude_hooks;
use crate::error::{Error, Result};
use crate::mail_settings::{pataka_command, run_pataka};
use crate::state::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarStatus {
    connected: bool,
    email: Option<String>,
    /// Offered to every Claude Code session through the service.
    registered: bool,
    service_installed: bool,
    can_register: bool,
}

fn vault(state: &AppState) -> Result<CalendarVault> {
    state.with_session(|s| {
        Ok(CalendarVault {
            store: state.store.clone(),
            key: s.key.clone(),
        })
    })
}

fn status_view(state: &AppState) -> Result<CalendarStatus> {
    let account = vault(state)?.load()?.account;
    Ok(CalendarStatus {
        connected: account.is_some(),
        email: account.map(|a| a.email),
        registered: claude_hooks::calendar_registered(),
        service_installed: state.store.root().join("service.json").exists(),
        can_register: pataka_command().is_some(),
    })
}

/// Open `url` in the person's browser.
fn open_browser(url: &str) -> bool {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    cmd.arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[tauri::command]
pub fn calendar_status(state: State<'_, AppState>) -> Result<CalendarStatus> {
    status_view(&state)
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SignInEvent {
    url: String,
    opened: bool,
}

/// Ask for the OAuth client file, then sign in to Google in the browser.
/// `None` if the person cancels choosing the file.
#[tauri::command]
pub async fn calendar_connect(app: AppHandle, state: State<'_, AppState>) -> Result<Option<CalendarStatus>> {
    use tauri_plugin_dialog::DialogExt;
    let v = vault(&state)?;
    let Some(file) = app
        .dialog()
        .file()
        .set_title("Choose the OAuth client file you downloaded from Google Cloud Console")
        .add_filter("OAuth client", &["json"])
        .blocking_pick_file()
    else {
        return Ok(None);
    };
    let path = file
        .into_path()
        .map_err(|_| Error::Invalid("That file cannot be used.".into()))?;
    let text =
        std::fs::read_to_string(&path).map_err(|_| Error::Invalid(format!("Could not read {}.", path.display())))?;
    let account = calendar::sign_in(&text, None, |url| {
        let opened = open_browser(url);
        let _ = app.emit(
            "calendar-signin",
            SignInEvent {
                url: url.to_string(),
                opened,
            },
        );
    })
    .await?;
    calendar::save_connected(&v, account)?;
    Ok(Some(status_view(&state)?))
}

/// Forget the account (Google is asked to revoke it), and stop offering it
/// to Claude Code. Says whether Google revoked it.
#[tauri::command]
pub async fn calendar_disconnect(state: State<'_, AppState>) -> Result<(CalendarStatus, bool)> {
    let v = vault(&state)?;
    let revoked = calendar::disconnect(&v).await?.unwrap_or(true);
    if claude_hooks::calendar_registered() {
        let _ = run_pataka(&["calendar", "unregister"]).await;
    }
    Ok((status_view(&state)?, revoked))
}

/// Offer the calendar to every Claude Code session, or stop.
#[tauri::command]
pub async fn calendar_register(state: State<'_, AppState>, on: bool) -> Result<CalendarStatus> {
    let st = status_view(&state)?;
    if on && !st.connected {
        return Err(Error::Invalid("Connect a calendar first.".into()));
    }
    if on && !st.service_installed {
        return Err(Error::Invalid(
            "This needs the Pātaka service. Install it from a terminal with `pataka service install`, then try again."
                .into(),
        ));
    }
    if on {
        run_pataka(&["calendar", "register", "--yes"]).await?;
    } else {
        run_pataka(&["calendar", "unregister"]).await?;
    }
    status_view(&state)
}
