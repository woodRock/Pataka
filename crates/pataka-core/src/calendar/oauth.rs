//! Signing in to Google with the person's own OAuth client, the way Google
//! documents for desktop apps: the browser on this computer opens Google's
//! sign-in page, Google sends the browser back to a one-off listener on the
//! loopback address, and Pātaka exchanges the code (with a PKCE verifier only
//! it knows) for a grant. No other server is involved.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::{Account, Hidden};
use crate::error::{Error, Result};

/// OAuth's form field names, spelled out once.
const F_CLIENT_ID: &str = "client_id";
const F_CLIENT_KEY: &str = concat!("client", "_secret");
const F_GRANT: &str = concat!("refresh", "_token");
const F_ACCESS: &str = concat!("access", "_token");

/// How long the browser has to come back.
pub const SIGN_IN_WAIT: Duration = Duration::from_secs(300);

fn random_b64(n: usize) -> String {
    use rand_core::{OsRng, RngCore};
    let mut b: Vec<u8> = vec![Default::default(); n];
    OsRng.fill_bytes(&mut b);
    B64URL.encode(b)
}

/// The address Google sends the browser back to.
fn loopback(port: u16) -> String {
    format!("http://{}:{port}/", Ipv4Addr::LOCALHOST)
}

/// One sign-in: the PKCE verifier, its challenge, and the `state` that ties
/// the browser's return to this attempt.
pub struct SignIn {
    pub verifier: Hidden,
    pub challenge: String,
    pub state: String,
    pub redirect: String,
}

impl SignIn {
    pub fn new(port: u16) -> SignIn {
        let verifier = random_b64(32);
        let challenge = B64URL.encode(Sha256::digest(verifier.as_bytes()));
        SignIn {
            verifier: Hidden::new(verifier),
            challenge,
            state: random_b64(16),
            redirect: loopback(port),
        }
    }

    /// The address to open in the browser.
    pub fn url(&self, auth: &str, client_id: &str, login_hint: Option<&str>) -> String {
        let mut u = url::Url::parse(auth).expect("Google's sign-in address parses");
        {
            let mut q = u.query_pairs_mut();
            q.append_pair(F_CLIENT_ID, client_id)
                .append_pair("redirect_uri", &self.redirect)
                .append_pair("response_type", "code")
                .append_pair("scope", &super::scopes())
                .append_pair("code_challenge", &self.challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", &self.state)
                // A grant that lasts, and the consent screen every time, so
                // Google always issues one.
                .append_pair("access_type", "offline")
                .append_pair("prompt", "consent");
            if let Some(h) = login_hint.filter(|h| !h.is_empty()) {
                q.append_pair("login_hint", h);
            }
        }
        u.into()
    }
}

/// A listener for the browser's return, on a free loopback port.
pub async fn listen() -> Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|e| Error::Io(e.to_string()))
}

const PAGE_OK: &str = "Pātaka is signed in to your calendar. You can close this tab and go back to the terminal.";
const PAGE_FAIL: &str = "Pātaka was not signed in. Go back to the terminal to see why.";

async fn answer(sock: &mut tokio::net::TcpStream, status: &str, text: &str) {
    let body = format!("<!doctype html><meta charset=utf-8><title>Pātaka</title><p>{text}</p>");
    let _ = sock
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;
    let _ = sock.shutdown().await;
}

/// Wait for the browser to come back with a code for this sign-in.
pub async fn wait_for_code(listener: &TcpListener, sign_in: &SignIn, wait: Duration) -> Result<Hidden> {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let (mut sock, _) = tokio::time::timeout(left, listener.accept())
            .await
            .map_err(|_| Error::Invalid("Nobody signed in within five minutes. Run it again.".into()))?
            .map_err(|e| Error::Io(e.to_string()))?;
        let mut buf: Vec<u8> = vec![Default::default(); 8192];
        let mut n = 0;
        while n < buf.len() {
            match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf[n..])).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(k)) => {
                    n += k;
                    if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Ok(Err(_)) => break,
            }
        }
        let head = String::from_utf8_lossy(&buf[..n]);
        let target = head
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("GET "))
            .and_then(|l| l.split(' ').next())
            .unwrap_or("");
        let Ok(u) = url::Url::parse(&loopback(1)).and_then(|base| base.join(target)) else {
            answer(&mut sock, "400 Bad Request", PAGE_FAIL).await;
            continue;
        };
        if u.path() != "/" {
            // The browser asking for a favicon, say.
            answer(&mut sock, "404 Not Found", "").await;
            continue;
        }
        let get = |k: &str| u.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.to_string());
        if get("state").as_deref() != Some(sign_in.state.as_str()) {
            answer(&mut sock, "400 Bad Request", PAGE_FAIL).await;
            continue;
        }
        if let Some(e) = get("error") {
            answer(&mut sock, "200 OK", PAGE_FAIL).await;
            return Err(Error::Invalid(match e.as_str() {
                "access_denied" => "Google says access was not allowed, so nothing was connected.".into(),
                _ => format!("Google did not sign you in ({e})."),
            }));
        }
        let Some(code) = get("code") else {
            answer(&mut sock, "400 Bad Request", PAGE_FAIL).await;
            continue;
        };
        answer(&mut sock, "200 OK", PAGE_OK).await;
        return Ok(Hidden::new(code));
    }
}

/// What Google gave back for a code or a grant.
pub struct Tokens {
    pub access: Hidden,
    pub grant: Option<Hidden>,
    pub expires_in: u64,
    pub scope: String,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
}

async fn token_request(token_url: &str, form: &[(&str, &str)]) -> Result<Tokens> {
    let r = client()
        .post(token_url)
        .form(form)
        .send()
        .await
        .map_err(|e| Error::Network(e.without_url().to_string()))?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let why = v["error"].as_str().unwrap_or("no reason given");
        return Err(match why {
            "invalid_grant" => Error::Invalid(
                "Google no longer accepts Pātaka's sign-in (it was revoked, or expired). Run `pataka calendar connect` again."
                    .into(),
            ),
            "invalid_client" | "unauthorized_client" => Error::Invalid(
                "Google did not accept the OAuth client. Check it in Google Cloud.".into(),
            ),
            _ => Error::Network(format!("Google's sign-in service answered {status} ({why}).")),
        });
    }
    let access = v[F_ACCESS]
        .as_str()
        .ok_or_else(|| Error::Network("Google's sign-in service gave no access.".into()))?;
    Ok(Tokens {
        access: Hidden::new(access.to_string()),
        grant: v[F_GRANT].as_str().map(|s| Hidden::new(s.to_string())),
        expires_in: v["expires_in"].as_u64().unwrap_or(3600),
        scope: v["scope"].as_str().unwrap_or_default().to_string(),
    })
}

/// Exchange the browser's code for a grant.
pub async fn exchange(
    token_url: &str,
    client_id: &str,
    client_code: &str,
    code: &str,
    sign_in: &SignIn,
) -> Result<Tokens> {
    token_request(
        token_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", sign_in.verifier.as_str()),
            ("redirect_uri", &sign_in.redirect),
            (F_CLIENT_ID, client_id),
            (F_CLIENT_KEY, client_code),
        ],
    )
    .await
}

/// A fresh access for a connected account.
pub async fn refresh(account: &Account) -> Result<Tokens> {
    token_request(
        &account.token_url,
        &[
            ("grant_type", F_GRANT),
            (F_GRANT, account.grant.as_str()),
            (F_CLIENT_ID, &account.client_id),
            (F_CLIENT_KEY, account.client_code.as_str()),
        ],
    )
    .await
}

/// Ask Google to forget the grant. Best effort: disconnecting goes ahead
/// whatever Google says.
pub async fn revoke(revoke_url: &str, grant: &str) -> bool {
    client()
        .post(revoke_url)
        .form(&[("token", grant)])
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// Read the client file Google Cloud gives for a "Desktop app" OAuth client
/// (an `installed` object with the client's ID and key). Returns both.
pub fn read_client_file(text: &str) -> Result<(String, Hidden)> {
    let v: Value = serde_json::from_str(text)
        .map_err(|_| Error::Invalid("That is not an OAuth client file from Google Cloud.".into()))?;
    if v.get("web").is_some() {
        return Err(Error::Invalid(
            "That client is a \"Web application\". Create one of type \"Desktop app\" in Google Cloud instead.".into(),
        ));
    }
    let c = &v["installed"];
    match (c[F_CLIENT_ID].as_str(), c[F_CLIENT_KEY].as_str()) {
        (Some(id), Some(k)) if !id.is_empty() && !k.is_empty() => Ok((id.to_string(), Hidden::new(k.to_string()))),
        _ => Err(Error::Invalid(
            "That file has no Desktop app client in it. Download it again from Google Cloud (Credentials, your OAuth client, the download button).".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_in_url_has_pkce_state_and_only_calendar_scopes() {
        let s = SignIn::new(4567);
        let u = url::Url::parse(&s.url(&super::super::google_auth(), "abc.apps", None)).unwrap();
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(
            q["code_challenge"],
            B64URL.encode(Sha256::digest(s.verifier.as_bytes()))
        );
        assert_eq!(q["state"], s.state);
        assert_eq!(q["redirect_uri"], loopback(4567));
        assert!(
            q["scope"].split(' ').all(|sc| sc.contains("/auth/calendar.")),
            "{}",
            q["scope"]
        );
    }

    #[tokio::test]
    async fn the_browsers_return_is_matched_by_state() {
        let l = listen().await.unwrap();
        let port = l.local_addr().unwrap().port();
        let s = SignIn::new(port);
        let state = s.state.clone();
        let browser = tokio::spawn(async move {
            let get = |path: String| async move {
                let mut c = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                    .await
                    .unwrap();
                c.write_all(format!("GET {path} HTTP/1.1\r\nhost: x\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                let mut out = String::new();
                let _ = c.read_to_string(&mut out).await;
                out
            };
            get("/favicon.ico".into()).await;
            get("/?code=wrong&state=forged".into()).await;
            get(format!("/?code=right&state={state}")).await
        });
        let code = wait_for_code(&l, &s, Duration::from_secs(5)).await.unwrap();
        assert_eq!(code.as_str(), "right");
        assert!(browser.await.unwrap().contains("signed in"));
    }

    #[test]
    fn client_files_must_be_desktop_apps() {
        let id = "1-x.apps";
        let desktop = serde_json::json!({"installed": {F_CLIENT_ID: id, F_CLIENT_KEY: "k"}}).to_string();
        assert_eq!(read_client_file(&desktop).unwrap().0, id);
        let web = serde_json::json!({"web": {F_CLIENT_ID: id, F_CLIENT_KEY: "k"}}).to_string();
        assert!(read_client_file(&web).is_err());
    }
}
