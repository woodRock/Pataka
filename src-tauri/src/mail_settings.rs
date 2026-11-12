//! Settings → Mail: connect a mailbox for Pātaka Mail from the app, the same
//! account `pataka mail connect` sets up in a terminal. The app password is
//! checked against the server, then kept encrypted in Pātaka's store.
//! Offering the mailbox to every Claude Code session runs the `pataka`
//! command, which owns the service and Claude Code's settings.

use serde::Serialize;
use tauri::State;
use zeroize::Zeroizing;

use crate::audit;
use crate::claude_hooks;
use crate::error::{Error, Result};
use crate::mail::ops::Mailer;
use crate::mail::{self, Account, MailVault};
use crate::state::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailStatus {
    connected: bool,
    email: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    /// Offered to every Claude Code session through the service.
    registered: bool,
    /// The Pātaka service is installed (`pataka service install`).
    service_installed: bool,
    /// A `pataka` command the app can run to change the registration.
    can_register: bool,
}

fn vault(state: &AppState) -> Result<MailVault> {
    state.with_session(|s| {
        Ok(MailVault {
            store: state.store.clone(),
            key: s.key.clone(),
        })
    })
}

/// The `pataka` command: the copy bundled with the app, or the installed one.
fn pataka_command() -> Option<std::path::PathBuf> {
    crate::terminal::bundled().or_else(|| crate::terminal::installed_path().filter(|p| p.exists()))
}

fn status_view(state: &AppState) -> Result<MailStatus> {
    let account = vault(state)?.load()?.account;
    Ok(MailStatus {
        connected: account.is_some(),
        email: account.as_ref().map(|a| a.email.clone()),
        host: account.as_ref().map(|a| a.host.clone()),
        port: account.as_ref().map(|a| a.port),
        registered: claude_hooks::mail_registered(),
        service_installed: state.store.root().join("service.json").exists(),
        can_register: pataka_command().is_some(),
    })
}

/// `host` or `host:port`; the port defaults to 993 (IMAP over TLS).
fn parse_host(h: &str) -> Result<(String, u16)> {
    let h = h.trim();
    let h = if h.is_empty() { mail::GMAIL_HOST } else { h };
    let (host, port) = match h.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse()
                .map_err(|_| Error::Invalid("The port must be a number.".into()))?,
        ),
        None => (h, mail::IMAPS_PORT),
    };
    if host.is_empty() || host.contains(['/', ' ', '@']) {
        return Err(Error::Invalid("That is not a server name.".into()));
    }
    Ok((host.to_lowercase(), port))
}

#[tauri::command]
pub fn mail_status(state: State<'_, AppState>) -> Result<MailStatus> {
    status_view(&state)
}

/// Check the login with the server, then store the account.
#[tauri::command]
pub async fn mail_connect(
    state: State<'_, AppState>,
    email: String,
    host: String,
    password: String,
) -> Result<MailStatus> {
    let password = Zeroizing::new(password);
    let email = email.trim().to_string();
    if !email.contains('@') || email.contains(char::is_whitespace) {
        return Err(Error::Invalid("Enter the mailbox's email address.".into()));
    }
    // App passwords are often shown in groups of four with spaces.
    let password = Zeroizing::new(password.chars().filter(|c| !c.is_whitespace()).collect::<String>());
    if password.is_empty() {
        return Err(Error::Invalid("Enter the app password.".into()));
    }
    let (host, port) = parse_host(&host)?;
    let v = vault(&state)?;
    let account = Account {
        email,
        password,
        host: host.clone(),
        port,
        tls: true,
    };
    let m = Mailer::connect(&account).await?;
    let gmail = m.gmail;
    m.close().await;
    v.set_account(Some(account))?;
    audit::append(
        &state.store.audit_path(),
        audit::Entry {
            event: "mail-connected".into(),
            provider: Some(host),
            detail: Some(if gmail { "Gmail".into() } else { "IMAP".into() }),
            ..Default::default()
        },
    )?;
    status_view(&state)
}

/// Forget the account, and stop offering it to Claude Code.
#[tauri::command]
pub async fn mail_disconnect(state: State<'_, AppState>) -> Result<MailStatus> {
    let v = vault(&state)?;
    let Some(account) = v.load()?.account else {
        return status_view(&state);
    };
    v.set_account(None)?;
    if claude_hooks::mail_registered() {
        let _ = run_pataka(&["mail", "unregister"]).await;
    }
    audit::append(
        &state.store.audit_path(),
        audit::Entry {
            event: "mail-disconnected".into(),
            provider: Some(account.host),
            ..Default::default()
        },
    )?;
    status_view(&state)
}

/// Offer the mailbox to every Claude Code session (`pataka mail register`),
/// or stop offering it (`pataka mail unregister`).
#[tauri::command]
pub async fn mail_register(state: State<'_, AppState>, on: bool) -> Result<MailStatus> {
    let st = status_view(&state)?;
    if on && !st.connected {
        return Err(Error::Invalid("Connect a mailbox first.".into()));
    }
    if on && !st.service_installed {
        return Err(Error::Invalid(
            "This needs the Pātaka service. Install it from a terminal with `pataka service install`, then try again."
                .into(),
        ));
    }
    if on {
        run_pataka(&["mail", "register", "--yes"]).await?;
    } else {
        run_pataka(&["mail", "unregister"]).await?;
    }
    status_view(&state)
}

async fn run_pataka(args: &[&str]) -> Result<()> {
    let exe = pataka_command().ok_or_else(|| {
        Error::Invalid("The pataka command is not available. Install it in Settings → Claude Code first.".into())
    })?;
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let out = tauri::async_runtime::spawn_blocking(move || std::process::Command::new(exe).args(&args).output())
        .await
        .map_err(|_| Error::Invalid("internal task failed".into()))??;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("it failed");
        Err(Error::Invalid(format!("pataka mail: {}", line.trim())))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_host;
    use crate::mail::GMAIL_HOST;

    #[test]
    fn hosts_default_to_gmail_and_port_993() {
        assert_eq!(parse_host("").unwrap(), (GMAIL_HOST.to_string(), 993));
        assert_eq!(
            parse_host(" IMAP.Example.org:1993 ").unwrap(),
            ("imap.example.org".into(), 1993)
        );
        assert!(parse_host("someone@example.org").is_err());
        assert!(parse_host("imap.example.org:abc").is_err());
    }
}
