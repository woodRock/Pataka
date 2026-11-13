//! A small IMAP4rev1 client: just the commands Pātaka Mail uses.
//!
//! TLS is rustls (TLS 1.2 or later, the operating system's roots with
//! Mozilla's as a fallback), the same stack as Pātaka's HTTP client. Plain
//! TCP is allowed only to a loopback address, for tests. Connections go only
//! to the server named in the stored account.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};

use crate::error::{Error, Result};

/// Longest single response line, and largest literal, Pātaka accepts.
const MAX_LINE: usize = 64 * 1024;
const MAX_LITERAL: usize = 64 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(60);

trait Conn: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Conn for T {}

/// One token of a server response.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Atom(String),
    Str(Vec<u8>),
    List(Vec<Tok>),
    Nil,
}

impl Tok {
    /// The text of an atom or string.
    pub fn text(&self) -> Option<String> {
        match self {
            Tok::Atom(a) => Some(a.clone()),
            Tok::Str(s) => Some(String::from_utf8_lossy(s).into_owned()),
            _ => None,
        }
    }
    pub fn list(&self) -> &[Tok] {
        match self {
            Tok::List(l) => l,
            _ => &[],
        }
    }
    pub fn is_atom(&self, a: &str) -> bool {
        matches!(self, Tok::Atom(x) if x.eq_ignore_ascii_case(a))
    }
}

/// A part of a command: plain text, or a string sent as a literal.
pub enum Part {
    Raw(String),
    Lit(Vec<u8>),
}

/// Quote a string for a command. Anything that is not plain ASCII, or holds
/// quotes, backslashes or line breaks, goes as a literal.
pub fn astring(s: &str) -> Part {
    if s.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\') {
        Part::Raw(format!("\"{s}\""))
    } else {
        Part::Lit(s.as_bytes().to_vec())
    }
}

/// Mailbox names travel in modified UTF-7 (RFC 3501 §5.1.3).
pub fn mailbox(name: &str) -> Part {
    astring(&utf7_encode(name))
}

pub fn utf7_encode(s: &str) -> String {
    use base64::Engine;
    let engine =
        base64::engine::GeneralPurpose::new(&base64::alphabet::IMAP_MUTF7, base64::engine::general_purpose::NO_PAD);
    let mut out = String::new();
    let mut pending: Vec<u16> = Vec::new();
    let flush = |pending: &mut Vec<u16>, out: &mut String| {
        if !pending.is_empty() {
            let bytes: Vec<u8> = pending.iter().flat_map(|u| u.to_be_bytes()).collect();
            out.push('&');
            out.push_str(&engine.encode(bytes));
            out.push('-');
            pending.clear();
        }
    };
    for c in s.chars() {
        if (' '..='~').contains(&c) {
            flush(&mut pending, &mut out);
            if c == '&' {
                out.push_str("&-");
            } else {
                out.push(c);
            }
        } else {
            let mut buf = [0u16; 2];
            pending.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush(&mut pending, &mut out);
    out
}

pub fn utf7_decode(s: &str) -> String {
    use base64::Engine;
    let engine =
        base64::engine::GeneralPurpose::new(&base64::alphabet::IMAP_MUTF7, base64::engine::general_purpose::NO_PAD);
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(end) = after.find('-') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let chunk = &after[..end];
        if chunk.is_empty() {
            out.push('&');
        } else if let Ok(bytes) = engine.decode(chunk) {
            let units: Vec<u16> = bytes
                .chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            out.push_str(&String::from_utf16_lossy(&units));
        } else {
            out.push('&');
            out.push_str(chunk);
            out.push('-');
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Parse the tokens of one response line (literals already inlined in
/// `{n}\r\n<bytes>` form, as read from the wire).
pub fn tokenize(b: &[u8]) -> Vec<Tok> {
    fn seq(b: &[u8], i: &mut usize, close: Option<u8>) -> Vec<Tok> {
        let mut out = Vec::new();
        while *i < b.len() {
            match b[*i] {
                b' ' | b'\r' | b'\n' => *i += 1,
                b')' | b']' if Some(b[*i]) == close => {
                    *i += 1;
                    return out;
                }
                b'(' => {
                    *i += 1;
                    out.push(Tok::List(seq(b, i, Some(b')'))));
                }
                b'"' => {
                    *i += 1;
                    let mut s = Vec::new();
                    while *i < b.len() && b[*i] != b'"' {
                        if b[*i] == b'\\' && *i + 1 < b.len() {
                            *i += 1;
                        }
                        s.push(b[*i]);
                        *i += 1;
                    }
                    *i += 1;
                    out.push(Tok::Str(s));
                }
                b'{' => {
                    let start = *i + 1;
                    let end = b[start..].iter().position(|&c| c == b'}').map(|p| start + p);
                    let n = end
                        .and_then(|e| std::str::from_utf8(&b[start..e]).ok())
                        .and_then(|s| s.trim_end_matches('+').parse::<usize>().ok());
                    match (end, n) {
                        (Some(e), Some(n)) => {
                            let mut j = e + 1;
                            if b.get(j) == Some(&b'\r') {
                                j += 1;
                            }
                            if b.get(j) == Some(&b'\n') {
                                j += 1;
                            }
                            let stop = (j + n).min(b.len());
                            out.push(Tok::Str(b[j..stop].to_vec()));
                            *i = stop;
                        }
                        _ => {
                            *i += 1;
                        }
                    }
                }
                _ => {
                    // An atom. Brackets nest, so `BODY[HEADER.FIELDS (FROM)]`
                    // and `[Gmail]/Trash` stay one atom.
                    let start = *i;
                    let mut depth = 0;
                    while *i < b.len() {
                        let c = b[*i];
                        if c == b'[' {
                            depth += 1;
                        } else if c == b']' {
                            if depth == 0 {
                                break;
                            }
                            depth -= 1;
                        } else if depth == 0 && matches!(c, b' ' | b'(' | b')' | b'\r' | b'\n' | b'"') {
                            break;
                        }
                        *i += 1;
                    }
                    if *i == start {
                        *i += 1;
                        continue;
                    }
                    let a = String::from_utf8_lossy(&b[start..*i]).into_owned();
                    out.push(if a.eq_ignore_ascii_case("NIL") {
                        Tok::Nil
                    } else {
                        Tok::Atom(a)
                    });
                }
            }
        }
        out
    }
    let mut i = 0;
    seq(b, &mut i, None)
}

/// Where to connect.
#[derive(Debug, Clone)]
pub struct Server {
    pub host: String,
    pub port: u16,
    /// Plain TCP: only allowed for loopback addresses (tests).
    pub tls: bool,
}

pub struct Imap {
    io: BufStream<Box<dyn Conn>>,
    tag: u32,
    pub caps: Vec<String>,
    /// Untagged responses that arrived with the last command.
    pub untagged: Vec<Vec<Tok>>,
    /// The text of the last tagged OK, e.g. `[COPYUID 1 2 3] done`.
    pub last_ok: String,
}

fn tls_config() -> Arc<tokio_rustls::rustls::ClientConfig> {
    use tokio_rustls::rustls;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .expect("TLS versions are supported")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(cfg)
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

fn net(what: &str) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Network(format!("{what} ({})", e.kind()))
}

impl Imap {
    pub async fn connect(server: &Server) -> Result<Imap> {
        if !server.tls && !is_loopback(&server.host) {
            return Err(Error::Invalid("Mail servers must use TLS.".into()));
        }
        let tcp = tokio::time::timeout(
            TIMEOUT,
            tokio::net::TcpStream::connect((server.host.as_str(), server.port)),
        )
        .await
        .map_err(|_| Error::Network("the mail server did not answer".into()))?
        .map_err(net("could not connect to the mail server"))?;
        let conn: Box<dyn Conn> = if server.tls {
            let name = tokio_rustls::rustls::pki_types::ServerName::try_from(server.host.clone())
                .map_err(|_| Error::Invalid("That is not a valid server name.".into()))?;
            let tls = tokio::time::timeout(
                TIMEOUT,
                tokio_rustls::TlsConnector::from(tls_config()).connect(name, tcp),
            )
            .await
            .map_err(|_| Error::Network("the TLS handshake timed out".into()))?
            .map_err(net("the TLS handshake failed"))?;
            Box::new(tls)
        } else {
            Box::new(tcp)
        };
        let mut imap = Imap {
            io: BufStream::new(conn),
            tag: 0,
            caps: Vec::new(),
            untagged: Vec::new(),
            last_ok: String::new(),
        };
        let greeting = imap.read_response().await?;
        if !greeting.starts_with(b"* OK") && !greeting.starts_with(b"* PREAUTH") {
            return Err(Error::Network("the mail server refused the connection".into()));
        }
        Ok(imap)
    }

    /// One response line, with any literals read and inlined.
    async fn read_response(&mut self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        loop {
            let start = buf.len();
            let n = tokio::time::timeout(
                TIMEOUT,
                (&mut self.io).take(MAX_LINE as u64).read_until(b'\n', &mut buf),
            )
            .await
            .map_err(|_| Error::Network("the mail server stopped answering".into()))?
            .map_err(net("reading from the mail server failed"))?;
            if n == 0 {
                return Err(Error::Network("the mail server closed the connection".into()));
            }
            let line = &buf[start..];
            if !line.ends_with(b"\n") {
                return Err(Error::Network("the mail server sent an over-long line".into()));
            }
            let trimmed = line
                .strip_suffix(b"\r\n")
                .or_else(|| line.strip_suffix(b"\n"))
                .unwrap_or(line);
            let lit = trimmed
                .strip_suffix(b"}")
                .and_then(|t| t.iter().rposition(|&c| c == b'{').map(|p| &t[p + 1..]))
                .and_then(|n| std::str::from_utf8(n).ok())
                .and_then(|n| n.trim_end_matches('+').parse::<usize>().ok());
            match lit {
                Some(n) if n <= MAX_LITERAL => {
                    let at = buf.len();
                    buf.resize(at + n, 0);
                    tokio::time::timeout(TIMEOUT, self.io.read_exact(&mut buf[at..]))
                        .await
                        .map_err(|_| Error::Network("the mail server stopped answering".into()))?
                        .map_err(net("reading from the mail server failed"))?;
                }
                Some(_) => return Err(Error::Network("the mail server sent an over-long message".into())),
                None => return Ok(buf),
            }
        }
    }

    async fn write(&mut self, b: &[u8]) -> Result<()> {
        self.io
            .write_all(b)
            .await
            .map_err(net("writing to the mail server failed"))?;
        self.io.flush().await.map_err(net("writing to the mail server failed"))
    }

    /// Send a command and collect its untagged responses. A NO or BAD answer
    /// is an error carrying the server's text.
    pub async fn run(&mut self, parts: Vec<Part>) -> Result<Vec<Vec<Tok>>> {
        self.tag += 1;
        let tag = format!("P{}", self.tag);
        let mut line = format!("{tag} ").into_bytes();
        for p in parts {
            match p {
                Part::Raw(s) => line.extend_from_slice(s.as_bytes()),
                Part::Lit(bytes) => {
                    line.extend_from_slice(format!("{{{}}}\r\n", bytes.len()).as_bytes());
                    self.write(&line).await?;
                    line.clear();
                    // Wait for the go-ahead.
                    loop {
                        let r = self.read_response().await?;
                        if r.starts_with(b"+") {
                            break;
                        }
                        if r.starts_with(tag.as_bytes()) {
                            return Err(Error::Network(server_text(&r, &tag)));
                        }
                    }
                    line.extend_from_slice(&bytes);
                }
            }
        }
        line.extend_from_slice(b"\r\n");
        self.write(&line).await?;
        let mut untagged = Vec::new();
        loop {
            let r = self.read_response().await?;
            if let Some(rest) = r.strip_prefix(b"* ") {
                untagged.push(tokenize(rest));
            } else if r.starts_with(tag.as_bytes()) {
                let rest = &r[tag.len()..];
                let rest = String::from_utf8_lossy(rest).trim().to_string();
                if rest.len() >= 2 && rest[..2].eq_ignore_ascii_case("OK") {
                    self.last_ok = rest[2..].trim().to_string();
                    self.untagged = untagged.clone();
                    return Ok(untagged);
                }
                return Err(Error::Network(server_text(&r, &tag)));
            }
            // A stray continuation: ignore.
        }
    }

    pub async fn raw(&mut self, cmd: &str) -> Result<Vec<Vec<Tok>>> {
        self.run(vec![Part::Raw(cmd.to_string())]).await
    }

    pub async fn login(&mut self, user: &str, pass: &str) -> Result<()> {
        self.run(vec![
            Part::Raw("LOGIN ".into()),
            astring(user),
            Part::Raw(" ".into()),
            astring(pass),
        ])
        .await
        .map_err(|e| {
            // The server's own reason (e.g. Gmail's "[AUTHENTICATIONFAILED]
            // Invalid credentials" or "[ALERT] Application-specific password
            // required"). It never contains the password.
            let why = match e {
                Error::Network(m) => m,
                other => other.to_string(),
            };
            Error::Invalid(format!(
                "The mail server did not accept that address and app password ({why})."
            ))
        })?;
        let caps = self.raw("CAPABILITY").await?;
        self.caps = caps
            .iter()
            .filter(|t| t.first().is_some_and(|a| a.is_atom("CAPABILITY")))
            .flat_map(|t| t[1..].iter().filter_map(|a| a.text()).map(|a| a.to_uppercase()))
            .collect();
        Ok(())
    }

    pub fn has(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c == cap)
    }

    pub fn is_gmail(&self) -> bool {
        self.has("X-GM-EXT-1")
    }

    pub async fn logout(&mut self) {
        let _ = self.raw("LOGOUT").await;
    }

    /// SELECT (read-write) or EXAMINE (read-only) a mailbox. Returns UIDVALIDITY.
    pub async fn open(&mut self, name: &str, write: bool) -> Result<u32> {
        let verb = if write { "SELECT " } else { "EXAMINE " };
        let r = self
            .run(vec![Part::Raw(verb.into()), mailbox(name)])
            .await
            .map_err(|_| Error::NotFound(format!("the mailbox {name}")))?;
        Ok(r.iter()
            .find_map(|t| {
                let code = t.get(1)?.text()?;
                let v = code.strip_prefix("[UIDVALIDITY ")?;
                v.trim_end_matches(']').parse().ok()
            })
            .unwrap_or(0))
    }

    /// `UID SEARCH`, returning UIDs in ascending order.
    pub async fn search(&mut self, criteria: Vec<Part>) -> Result<Vec<u32>> {
        let mut parts = vec![Part::Raw("UID SEARCH ".into())];
        parts.extend(criteria);
        let r = self.run(parts).await?;
        let mut uids: Vec<u32> = r
            .iter()
            .filter(|t| t.first().is_some_and(|a| a.is_atom("SEARCH")))
            .flat_map(|t| t[1..].iter().filter_map(|a| a.text()?.parse().ok()))
            .collect();
        uids.sort_unstable();
        uids.dedup();
        Ok(uids)
    }

    /// `UID FETCH`: one map of item name → value per message.
    pub async fn fetch(&mut self, uids: &[u32], items: &str) -> Result<Vec<Fetched>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let r = self.raw(&format!("UID FETCH {} ({items})", uid_set(uids))).await?;
        Ok(r.iter()
            .filter(|t| t.get(1).is_some_and(|a| a.is_atom("FETCH")))
            .filter_map(|t| t.get(2))
            .map(|l| {
                let items = l.list();
                let mut f = Fetched::default();
                for pair in items.chunks(2) {
                    if let [Tok::Atom(k), v] = pair {
                        f.items.push((k.to_uppercase(), v.clone()));
                    }
                }
                f
            })
            .filter(|f| f.uid().is_some())
            .collect())
    }

    /// LIST "" "*": (name, flags, delimiter).
    pub async fn list(&mut self) -> Result<Vec<(String, Vec<String>, String)>> {
        let r = self.raw("LIST \"\" \"*\"").await?;
        Ok(r.iter()
            .filter(|t| t.first().is_some_and(|a| a.is_atom("LIST")))
            .filter_map(|t| {
                let flags = t.get(1)?.list().iter().filter_map(|f| f.text()).collect();
                let delim = t.get(2).and_then(|d| d.text()).unwrap_or_else(|| "/".into());
                let name = utf7_decode(&t.get(3)?.text()?);
                Some((name, flags, delim))
            })
            .collect())
    }
}

fn server_text(r: &[u8], tag: &str) -> String {
    let s = String::from_utf8_lossy(r);
    let s = s.trim().strip_prefix(tag).unwrap_or(&s).trim().to_string();
    let s: String = s.chars().take(200).collect();
    format!("the mail server said: {s}")
}

/// One message from a FETCH response.
#[derive(Debug, Default, Clone)]
pub struct Fetched {
    pub items: Vec<(String, Tok)>,
}

impl Fetched {
    pub fn get(&self, key: &str) -> Option<&Tok> {
        self.items
            .iter()
            .find(|(k, _)| k == key || k.starts_with(&format!("{key}<")))
            .map(|(_, v)| v)
    }
    pub fn uid(&self) -> Option<u32> {
        self.get("UID")?.text()?.parse().ok()
    }
    pub fn msgid(&self) -> Option<u64> {
        self.get("X-GM-MSGID")?.text()?.parse().ok()
    }
    /// Gmail's thread id (X-GM-THRID).
    pub fn thrid(&self) -> Option<u64> {
        self.get("X-GM-THRID")?.text()?.parse().ok()
    }
    pub fn labels(&self) -> Vec<String> {
        self.get("X-GM-LABELS")
            .map(|l| {
                l.list()
                    .iter()
                    .filter_map(|t| t.text())
                    .map(|t| utf7_decode(&t))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn flags(&self) -> Vec<String> {
        self.get("FLAGS")
            .map(|l| l.list().iter().filter_map(|t| t.text()).collect())
            .unwrap_or_default()
    }
    /// The bytes of one fetched section, e.g. `BODY[2]`.
    pub fn section(&self, item: &str) -> Option<&[u8]> {
        match self.get(item)? {
            Tok::Str(b) => Some(b),
            _ => None,
        }
    }
    pub fn body(&self) -> Option<&[u8]> {
        match self.get("BODY[]")? {
            Tok::Str(b) => Some(b),
            _ => None,
        }
    }
}

/// `1,2,5:9` from a list of UIDs.
pub fn uid_set(uids: &[u32]) -> String {
    let mut v = uids.to_vec();
    v.sort_unstable();
    v.dedup();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let start = v[i];
        let mut end = start;
        while i + 1 < v.len() && v[i + 1] == end + 1 {
            i += 1;
            end = v[i];
        }
        out.push(if start == end {
            start.to_string()
        } else {
            format!("{start}:{end}")
        });
        i += 1;
    }
    out.join(",")
}

/// `[APPENDUID validity uid]` from an APPEND: the new message's UID.
pub fn appenduid(imap: &Imap) -> Option<u32> {
    let t = &imap.last_ok;
    let rest = &t[t.find("[APPENDUID ")? + 11..];
    rest.split(']').next()?.split_whitespace().nth(1)?.parse().ok()
}

/// `[COPYUID validity src dst]` from a MOVE or COPY: pairs of (old, new) UIDs.
pub fn copyuid(imap: &Imap) -> Vec<(u32, u32)> {
    fn expand(set: &str) -> Vec<u32> {
        set.split(',')
            .flat_map(|r| match r.split_once(':') {
                Some((a, b)) => {
                    let (a, b): (u32, u32) = (a.parse().unwrap_or(0), b.parse().unwrap_or(0));
                    let (lo, hi) = (a.min(b), a.max(b));
                    (lo..=hi).collect::<Vec<_>>()
                }
                None => r.parse().ok().into_iter().collect(),
            })
            .collect()
    }
    let mut texts: Vec<String> = imap
        .untagged
        .iter()
        .filter(|t| t.first().is_some_and(|a| a.is_atom("OK")))
        .map(|t| t[1..].iter().filter_map(|x| x.text()).collect::<Vec<_>>().join(" "))
        .collect();
    texts.push(imap.last_ok.clone());
    for t in texts {
        if let Some(rest) = t.find("[COPYUID ").map(|i| &t[i + 9..]) {
            let rest = rest.split(']').next().unwrap_or("");
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() == 3 {
                let (src, dst) = (expand(parts[1]), expand(parts[2]));
                if src.len() == dst.len() {
                    return src.into_iter().zip(dst).collect();
                }
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenises_fetch_with_literal() {
        let raw = b"1 FETCH (UID 7 X-GM-MSGID 123 X-GM-LABELS (\"\\\\Inbox\" Work) BODY[]<0> {5}\r\nhello FLAGS (\\Seen))\r\n";
        let t = tokenize(raw);
        assert_eq!(t[0], Tok::Atom("1".into()));
        let items = t[2].list();
        assert_eq!(items[0], Tok::Atom("UID".into()));
        assert_eq!(items[7], Tok::Str(b"hello".to_vec()));
        assert_eq!(items[6], Tok::Atom("BODY[]<0>".into()));
    }

    #[test]
    fn utf7_round_trip() {
        for s in ["Whānau", "Inbox", "A&B", "[Gmail]/Trash", "Ngā mahi/Hui"] {
            assert_eq!(utf7_decode(&utf7_encode(s)), s);
        }
        assert_eq!(utf7_encode("A&B"), "A&-B");
    }

    #[test]
    fn uid_sets() {
        assert_eq!(uid_set(&[5, 1, 2, 3, 9]), "1:3,5,9");
    }
}
