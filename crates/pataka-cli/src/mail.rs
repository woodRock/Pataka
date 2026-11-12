//! `pataka mail`: connect a mailbox for Pātaka Mail, and undo its changes.
//!
//! The account (address, IMAP app password, server) is kept encrypted in
//! Pātaka's store. `pataka claude` then registers the `pataka-mail` MCP
//! server with Claude Code. See docs/CLAUDE-CODE.md, "Mail".

use pataka_core::audit;
use pataka_core::error::{Error, Result};
use pataka_core::mail::ops::Mailer;
use pataka_core::mail::{self, Account, MailVault, Record};
use zeroize::Zeroizing;

use crate::ctx::{self, Ctx};

fn vault(ctx: &Ctx) -> MailVault {
    MailVault {
        store: ctx.store.clone(),
        key: ctx.key.clone(),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| Error::Invalid("Could not start.".into()))
}

/// `host` or `host:port`; the port defaults to 993 (IMAP over TLS).
fn parse_host(h: &str) -> Result<(String, u16)> {
    let h = h.trim();
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

pub fn connect(host: Option<String>, email: Option<String>) -> Result<i32> {
    let ctx = Ctx::open()?;
    let (host, port) = parse_host(host.as_deref().unwrap_or(mail::GMAIL_HOST))?;
    let email = match email {
        Some(e) => e,
        None => ctx::prompt_line("Email address: "),
    };
    let email = email.trim().to_string();
    if email.is_empty() {
        return Err(Error::Invalid("No address was given.".into()));
    }
    if host == mail::GMAIL_HOST {
        eprintln!("Use a Gmail app password (https://myaccount.google.com/apppasswords), not your Google password.");
    }
    let password: Zeroizing<String> = ctx::prompt_secret("IMAP app password (not shown): ")?;
    let password = Zeroizing::new(password.split_whitespace().collect::<String>());
    let account = Account {
        email,
        password,
        host: host.clone(),
        port,
        tls: true,
    };
    eprintln!("Checking the login with {host}:{port} …");
    let gmail = runtime()?.block_on(async {
        let m = Mailer::connect(&account).await?;
        let g = m.gmail;
        m.close().await;
        Ok::<bool, Error>(g)
    })?;
    vault(&ctx).set_account(Some(account))?;
    audit::append(
        &ctx.store.audit_path(),
        audit::Entry {
            event: "mail-connected".into(),
            provider: Some(host.clone()),
            detail: Some(if gmail { "Gmail".into() } else { "IMAP".into() }),
            ..Default::default()
        },
    )?;
    println!(
        "Connected to {host}{}. `pataka claude` now offers Claude Code the pataka-mail tools.",
        if gmail { " (Gmail)" } else { "" }
    );
    println!("Claude Code only ever sees redacted text; changes are logged and `pataka mail undo` reverses them.");
    Ok(0)
}

pub fn disconnect() -> Result<i32> {
    let ctx = Ctx::open()?;
    let v = vault(&ctx);
    let Some(account) = v.load()?.account else {
        println!("No mailbox is connected.");
        return Ok(0);
    };
    v.set_account(None)?;
    audit::append(
        &ctx.store.audit_path(),
        audit::Entry {
            event: "mail-disconnected".into(),
            provider: Some(account.host.clone()),
            ..Default::default()
        },
    )?;
    println!(
        "Disconnected. The app password is no longer stored by Pātaka; you can revoke it in your account settings too."
    );
    Ok(0)
}

pub fn status() -> Result<i32> {
    let ctx = Ctx::open()?;
    match vault(&ctx).load()?.account {
        Some(a) => println!("Connected: {} on {}:{}", a.email, a.host, a.port),
        None => println!("No mailbox is connected. Connect one with `pataka mail connect`."),
    }
    Ok(0)
}

/// Whether a mailbox is connected (for `pataka claude`).
pub fn connected(ctx: &Ctx) -> bool {
    vault(ctx).connected().unwrap_or(false)
}

pub fn undo(seq: u64) -> Result<i32> {
    let ctx = Ctx::open()?;
    let path = ctx.store.audit_path();
    if !audit::verify(&path).ok {
        return Err(Error::Invalid(
            "The audit log failed its hash-chain check, so Pātaka will not act on it. See `pataka audit verify`."
                .into(),
        ));
    }
    let entries = audit::all(&path)?;
    let entry = entries
        .iter()
        .find(|e| e.seq == seq)
        .ok_or_else(|| Error::NotFound(format!("audit entry {seq}")))?;
    let record: Record = entry
        .mail
        .clone()
        .filter(|_| entry.event.starts_with("mail-"))
        .and_then(|m| serde_json::from_value(m).ok())
        .ok_or_else(|| Error::Invalid(format!("Entry {seq} is not a Pātaka Mail change.")))?;
    if let Some(done) = entries
        .iter()
        .find(|e| e.mail.as_ref().is_some_and(|m| m["undoes"].as_u64() == Some(seq)))
    {
        return Err(Error::Invalid(format!(
            "Entry {seq} was already undone (entry {}).",
            done.seq
        )));
    }
    let v = vault(&ctx);
    let account = v.account()?;
    if entry.provider.as_deref().is_some_and(|p| p != account.host) {
        return Err(Error::Invalid(
            "That change was made on a different mail server than the one connected now.".into(),
        ));
    }
    let (out, text) = runtime()?.block_on(async {
        let mut m = Mailer::connect(&account).await?;
        let r = m.undo(&v, &record, seq).await;
        m.close().await;
        r
    })?;
    let n = mail::audit_record(&path, entry.korero.as_deref(), &account.host, &out)?;
    println!("{text} (Logged as entry {n}.)");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert_eq!(parse_host("imap.gmail.com").unwrap(), ("imap.gmail.com".into(), 993));
        assert_eq!(
            parse_host("Mail.Example.nz:1993").unwrap(),
            ("mail.example.nz".into(), 1993)
        );
        assert!(parse_host("https://x").is_err());
    }
}
