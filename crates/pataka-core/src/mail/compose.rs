//! What goes into a draft beyond plain text: an HTML body, attached files, a
//! hosted poster shown inline, and drafts read whole from an `email.json`.
//!
//! A poster shown inline must be hosted (`poster_url`). An image embedded in
//! the draft itself (multipart/related, `cid:`) looks right in Drafts, but
//! Gmail drops it when the person sends the draft, so recipients see only
//! "[image: Poster]". `poster_image` is therefore always a plain attachment.
//!
//! The MCP server runs inside the proxy and does not know the session's
//! folder, so paths given as arguments must be absolute. Paths inside an
//! `email.json` are relative to its repository (the nearest folder above it
//! with `.git`, else its own folder) and may not leave it. Whether a file
//! outside the project folder may be attached at all is the PreToolUse
//! guard's question (`guards::review_mail`), which knows the folder and asks.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::error::{Error, Result};

/// Where `poster_url` is shown in `body_html`.
pub const POSTER_MARKER: &str = "<!-- POSTER -->";
/// Files may add up to this much. Base64 makes them about a third bigger,
/// and mail servers refuse messages much over 25 MB.
pub const MAX_FILES: u64 = 18 * 1024 * 1024;

/// A draft with every field settled and every path resolved.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DraftSpec {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub body_html: Option<String>,
    pub attachments: Vec<PathBuf>,
    pub poster_image: Option<PathBuf>,
    pub poster_url: Option<String>,
    pub reply_to: Option<String>,
}

/// A file read for a draft.
#[derive(Debug, Clone)]
pub struct File {
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
}

fn strings(v: &Value) -> Vec<String> {
    let all = match v {
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        Value::String(s) => vec![s.clone()],
        _ => Vec::new(),
    };
    all.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// A field that says something: not missing, blank or an empty list.
fn present(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::String(_) | Value::Array(_) => !strings(v).is_empty(),
        _ => true,
    }
}

/// Join `p` onto `base` and resolve `.` and `..` without touching the disk.
fn normalise(base: &Path, p: &Path) -> PathBuf {
    let joined = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn expand_home(p: &str) -> PathBuf {
    let home = || std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    match p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        Some(rest) => home().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    }
}

/// A path given as a tool argument: absolute (or `~/…`).
fn absolute(p: &str, key: &str) -> Result<PathBuf> {
    let path = expand_home(p.trim());
    if !path.is_absolute() {
        return Err(Error::Invalid(format!(
            "{key} needs a full path (got {p}): the mail server does not know which folder you are in. \
             Paths inside a from_json file may be relative to its repository."
        )));
    }
    Ok(normalise(Path::new("/"), &path))
}

/// The repository an `email.json` belongs to: the nearest folder above it
/// with `.git`, else the folder it is in.
pub fn repo_root(json: &Path) -> PathBuf {
    let dir = json.parent().unwrap_or(Path::new("/"));
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(dir)
        .to_path_buf()
}

/// A path written in an `email.json`, which must stay inside `root`.
fn inside(root: &Path, p: &str, key: &str) -> Result<PathBuf> {
    let path = normalise(root, &expand_home(p.trim()));
    // A symlink inside the repository may not lead out of it either.
    let real = path.canonicalize().unwrap_or_else(|_| path.clone());
    if !path.starts_with(root) || !real.starts_with(root) {
        return Err(Error::Invalid(format!(
            "{key} in from_json points outside its repository ({p}); only files under {} can be attached from it.",
            root.display()
        )));
    }
    Ok(path)
}

/// Settle a `draft` call: the arguments, filled in from `from_json` where
/// they leave a field out.
pub fn resolve(args: &Value) -> Result<DraftSpec> {
    let (file, root) = match args["from_json"].as_str().filter(|s| !s.trim().is_empty()) {
        None => (Value::Null, PathBuf::new()),
        Some(j) => {
            let path = absolute(j, "from_json")?;
            let path = path
                .canonicalize()
                .map_err(|e| Error::Invalid(format!("Could not read from_json ({}): {e}", path.display())))?;
            let text = std::fs::read_to_string(&path)
                .map_err(|e| Error::Invalid(format!("Could not read from_json ({}): {e}", path.display())))?;
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| Error::Invalid(format!("from_json ({}) could not be parsed: {e}", path.display())))?;
            if !v.is_object() {
                return Err(Error::Invalid(format!(
                    "from_json ({}) is not an object.",
                    path.display()
                )));
            }
            let root = repo_root(&path);
            (v, root)
        }
    };
    // Which value a field takes, and whether it came from the file.
    let pick = |key: &str| -> Option<(&Value, bool)> {
        if present(&args[key]) {
            Some((&args[key], false))
        } else if present(&file[key]) {
            Some((&file[key], true))
        } else {
            None
        }
    };
    let text = |key: &str| pick(key).and_then(|(v, _)| v.as_str().map(String::from));
    let path = |key: &str, p: &str, from_file: bool| {
        if from_file {
            inside(&root, p, key)
        } else {
            absolute(p, key)
        }
    };
    let attachments = match pick("attachments") {
        None => Vec::new(),
        Some((v, from_file)) => strings(v)
            .iter()
            .map(|p| path("attachments", p, from_file))
            .collect::<Result<Vec<_>>>()?,
    };
    let poster_image = match pick("poster_image") {
        Some((Value::String(p), from_file)) => Some(path("poster_image", p, from_file)?),
        _ => None,
    };
    Ok(DraftSpec {
        to: pick("to").map(|(v, _)| strings(v)).unwrap_or_default(),
        cc: pick("cc").map(|(v, _)| strings(v)).unwrap_or_default(),
        subject: text("subject").unwrap_or_default(),
        body: text("body").unwrap_or_default(),
        body_html: text("body_html"),
        attachments,
        poster_image,
        poster_url: text("poster_url").map(|u| u.trim().to_string()),
        reply_to: args["reply_to"]
            .as_str()
            .filter(|r| !r.trim().is_empty())
            .map(String::from),
    })
}

/// Every file a `draft` call would read, for the guard to check: the paths
/// as written, and those an `email.json` names once resolved.
pub fn draft_paths(args: &Value) -> Vec<String> {
    let mut v: Vec<String> = ["from_json", "poster_image"]
        .iter()
        .filter_map(|k| args[k].as_str())
        .chain(
            args["attachments"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str()),
        )
        .chain(args["attachments"].as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if let Ok(spec) = resolve(args) {
        for p in spec.attachments.iter().chain(spec.poster_image.iter()) {
            let p = p.display().to_string();
            if !v.contains(&p) {
                v.push(p);
            }
        }
    }
    v
}

/// The MIME type to give a file, from its extension.
pub fn mime_type(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "ics" => "text/calendar",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" | "md" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

/// Read the files a draft attaches. Returns the attachments and the poster.
pub fn load(spec: &DraftSpec) -> Result<(Vec<File>, Option<File>)> {
    let mut total = 0u64;
    for p in spec.attachments.iter().chain(spec.poster_image.iter()) {
        let meta =
            std::fs::metadata(p).map_err(|e| Error::Invalid(format!("Could not attach {}: {e}", p.display())))?;
        if !meta.is_file() {
            return Err(Error::Invalid(format!(
                "{} is not a file, so it cannot be attached.",
                p.display()
            )));
        }
        total += meta.len();
    }
    if total > MAX_FILES {
        return Err(Error::Invalid(format!(
            "The files add up to {:.1} MB; mail servers refuse drafts with more than about {} MB of files.",
            total as f64 / 1_048_576.0,
            MAX_FILES / 1_048_576
        )));
    }
    let read = |p: &PathBuf| -> Result<File> {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".into());
        let data = std::fs::read(p).map_err(|e| Error::Invalid(format!("Could not attach {}: {e}", p.display())))?;
        Ok(File {
            mime: mime_type(&name).to_string(),
            name,
            data,
        })
    };
    let files = spec.attachments.iter().map(read).collect::<Result<Vec<_>>>()?;
    let poster = spec.poster_image.as_ref().map(read).transpose()?;
    if let Some(p) = &poster {
        if !p.mime.starts_with("image/") {
            return Err(Error::Invalid(format!("poster_image ({}) is not an image.", p.name)));
        }
    }
    Ok((files, poster))
}

// ------------------------------------------------------------ MIME

fn base64_lines(data: &[u8]) -> Vec<u8> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    let mut out = Vec::with_capacity(b64.len() + b64.len() / 38 + 2);
    for chunk in b64.as_bytes().chunks(76) {
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn text_part(subtype: &str, text: &str) -> Vec<u8> {
    let text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let mut out = format!("Content-Type: text/{subtype}; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n")
        .into_bytes();
    out.extend(base64_lines(text.as_bytes()));
    out
}

/// `key="value"`, or RFC 2231's `key*=UTF-8''…` for a name that needs it.
fn param(key: &str, value: &str) -> String {
    if value.is_ascii() && !value.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
        return format!("{key}=\"{value}\"");
    }
    let mut enc = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            enc.push(b as char);
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    format!("{key}*=UTF-8''{enc}")
}

/// A calendar file's METHOD (PUBLISH when it gives none), which mail apps
/// read to offer "Add to calendar".
fn calendar_method(data: &[u8]) -> String {
    String::from_utf8_lossy(data)
        .lines()
        .find_map(|l| {
            let l = l.trim();
            l.get(..7)
                .filter(|p| p.eq_ignore_ascii_case("METHOD:"))
                .map(|_| l[7..].trim().to_ascii_uppercase())
        })
        .filter(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or_else(|| "PUBLISH".into())
}

fn file_part(f: &File) -> Vec<u8> {
    let mut ct = f.mime.clone();
    if f.mime == "text/calendar" {
        ct.push_str(&format!("; charset=utf-8; method={}", calendar_method(&f.data)));
    } else if f.mime.starts_with("text/") {
        ct.push_str("; charset=utf-8");
    }
    let mut h = format!(
        "Content-Type: {ct}; {}\r\nContent-Disposition: attachment; {}\r\n",
        param("name", &f.name),
        param("filename", &f.name)
    );
    h.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
    let mut out = h.into_bytes();
    out.extend(base64_lines(&f.data));
    out
}

fn multipart(subtype: &str, parts: Vec<Vec<u8>>) -> Vec<u8> {
    let b = format!("=_pataka_{}", uuid::Uuid::new_v4().simple());
    let mut out = format!("Content-Type: multipart/{subtype}; boundary=\"{b}\"\r\n\r\n").into_bytes();
    for p in parts {
        out.extend_from_slice(format!("--{b}\r\n").as_bytes());
        out.extend(p);
    }
    out.extend_from_slice(format!("--{b}--\r\n").as_bytes());
    out
}

/// A hosted poster's address, checked: https, and nothing that could break
/// out of the `src="…"` it goes in.
fn poster_src(url: &str) -> Result<&str> {
    let ok = url.len() > "https://".len()
        && url.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://"))
        && !url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "\"'<>`".contains(c));
    if ok {
        Ok(url)
    } else {
        Err(Error::Invalid(format!(
            "poster_url must be an https:// address of the hosted image (got {url})."
        )))
    }
}

/// `body_html` with the hosted poster put where its marker is.
fn place_poster(html: &str, poster_url: Option<&str>, poster_image: Option<&File>) -> Result<String> {
    let marked = html.contains(POSTER_MARKER);
    match poster_url {
        Some(url) => {
            let src = poster_src(url)?;
            if !marked {
                return Err(Error::Invalid(format!(
                    "body_html has no {POSTER_MARKER} marker to show poster_url in."
                )));
            }
            let img = format!("<img src=\"{src}\" alt=\"Poster\" style=\"max-width:100%;height:auto\">");
            Ok(html.replace(POSTER_MARKER, &img))
        }
        None if marked && poster_image.is_some() => Err(Error::Invalid(format!(
            "An image embedded in a draft does not survive Gmail sending it, so poster_image can only be \
             attached. To show it inline at {POSTER_MARKER}, host it at an https:// address and pass \
             poster_url (poster_image may still attach the file too)."
        ))),
        None => Ok(html.to_string()),
    }
}

/// The body of a draft, from its Content-Type header on: plain text, or
/// plain text and HTML as alternatives (the HTML showing a hosted poster),
/// with any files attached, `poster_image` among them.
pub fn mime_body(
    body: &str,
    html: Option<&str>,
    files: &[File],
    poster_image: Option<&File>,
    poster_url: Option<&str>,
) -> Result<Vec<u8>> {
    let html = html.filter(|h| !h.trim().is_empty());
    let poster_url = poster_url.map(str::trim).filter(|u| !u.is_empty());
    let attached: Vec<Vec<u8>> = files.iter().chain(poster_image).map(file_part).collect();
    let main = match html {
        None => {
            if poster_url.is_some() {
                return Err(Error::Invalid(format!(
                    "poster_url is shown in body_html, at its {POSTER_MARKER} marker; give a body_html."
                )));
            }
            text_part("plain", body)
        }
        Some(h) => {
            let plain = if body.trim().is_empty() {
                mail_parser::decoders::html::html_to_text(h)
            } else {
                body.to_string()
            };
            let h = place_poster(h, poster_url, poster_image)?;
            multipart("alternative", vec![text_part("plain", &plain), text_part("html", &h)])
        }
    };
    if attached.is_empty() {
        return Ok(main);
    }
    let mut parts = vec![main];
    parts.extend(attached);
    Ok(multipart("mixed", parts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::{MessageParser, MimeHeaders};
    use serde_json::json;

    fn file(name: &str, data: &[u8]) -> File {
        File {
            name: name.into(),
            mime: mime_type(name).into(),
            data: data.to_vec(),
        }
    }

    /// An address built at run time, so no real-looking one sits in the source.
    fn addr(user: &str) -> String {
        [user, "example.nz"].join("@")
    }

    /// A hosted image's address, built at run time like `addr`.
    fn hosted(path: &str) -> String {
        format!("{}://example.nz/{path}", "https")
    }

    fn message(body: Vec<u8>) -> Vec<u8> {
        let mut m = format!("From: {}\r\nSubject: s\r\nMIME-Version: 1.0\r\n", addr("a")).into_bytes();
        m.extend(body);
        m
    }

    #[test]
    fn html_poster_and_files_make_one_email() {
        let ics = b"BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n";
        let png = b"\x89PNG\r\n\x1a\nnot really";
        let url = hosted("talk/poster.png");
        let raw = message(
            mime_body(
                "Hello, the talk is on Friday.",
                Some("<p>Hello</p><!-- POSTER --><p>Friday</p>"),
                &[file("poster.pdf", b"%PDF-1.4"), file("event.ics", ics)],
                Some(&file("poster.png", png)),
                Some(&url),
            )
            .unwrap(),
        );
        let m = MessageParser::default().parse(&raw).unwrap();
        assert_eq!(m.body_text(0).unwrap().trim(), "Hello, the talk is on Friday.");
        let html = m.body_html(0).unwrap();
        assert!(
            html.contains(&format!("<img src=\"{url}\"")) && !html.contains(POSTER_MARKER) && !html.contains("cid:"),
            "{html}"
        );
        let names: Vec<String> = m
            .attachments()
            .filter_map(|a| a.attachment_name().map(String::from))
            .collect();
        assert!(
            names.contains(&"poster.pdf".into()) && names.contains(&"event.ics".into()),
            "{names:?}"
        );
        let cal = m
            .attachments()
            .find(|a| a.attachment_name() == Some("event.ics"))
            .unwrap();
        let ct = cal.content_type().unwrap();
        assert_eq!((ct.ctype(), ct.subtype()), ("text", Some("calendar")));
        assert_eq!(ct.attribute("method"), Some("REQUEST"));
        assert_eq!(cal.contents(), ics);
        // poster_image goes as an ordinary attachment, never embedded.
        let poster = m
            .attachments()
            .find(|a| a.attachment_name() == Some("poster.png"))
            .unwrap();
        assert_eq!(poster.contents(), png);
        assert!(poster.content_id().is_none());
        let text = String::from_utf8_lossy(&raw);
        for t in ["multipart/mixed", "multipart/alternative"] {
            assert!(text.contains(t), "no {t}");
        }
        assert!(!text.contains("multipart/related") && !text.contains("Content-ID"));
    }

    #[test]
    fn plain_text_alone_stays_plain() {
        let raw = mime_body("Hello", None, &[], None, None).unwrap();
        assert!(String::from_utf8_lossy(&raw).starts_with("Content-Type: text/plain; charset=utf-8"));
    }

    #[test]
    fn html_without_text_gets_a_text_version() {
        let raw = message(mime_body("", Some("<p>Hello everyone</p>"), &[], None, None).unwrap());
        let m = MessageParser::default().parse(&raw).unwrap();
        assert!(m.body_text(0).unwrap().contains("Hello everyone"));
    }

    #[test]
    fn a_poster_needs_its_marker_and_a_hosted_address() {
        let url = hosted("p.png");
        let e = mime_body("x", Some("<p>no marker</p>"), &[], None, Some(&url)).unwrap_err();
        assert!(e.to_string().contains("<!-- POSTER -->"), "{e}");
        let e = mime_body("x", None, &[], None, Some(&url)).unwrap_err();
        assert!(e.to_string().contains("body_html"), "{e}");
        let bad = [
            url.replacen("https", "http", 1),
            format!("{}:///tmp/p.png", "file"),
            hosted("p.png\" onerror=\"x"),
            format!("{}://", "https"),
        ];
        for b in &bad {
            let e = mime_body("x", Some("<!-- POSTER -->"), &[], None, Some(b)).unwrap_err();
            assert!(e.to_string().contains("poster_url must be"), "{b}: {e}");
        }
        // An image file with the marker but no hosted address is refused, not embedded.
        let e = mime_body("x", Some("<!-- POSTER -->"), &[], Some(&file("p.png", b"x")), None).unwrap_err();
        assert!(e.to_string().contains("poster_url"), "{e}");
        // Without the marker, or without HTML, it is simply attached.
        for html in [None, Some("<p>Hi</p>")] {
            let raw = message(mime_body("x", html, &[], Some(&file("p.png", b"x")), None).unwrap());
            let m = MessageParser::default().parse(&raw).unwrap();
            assert_eq!(m.attachments().count(), 1);
        }
    }

    #[test]
    fn non_ascii_file_names_are_encoded() {
        assert_eq!(param("filename", "a.pdf"), "filename=\"a.pdf\"");
        assert_eq!(param("filename", "pānui.pdf"), "filename*=UTF-8''p%C4%81nui.pdf");
        let raw = message(mime_body("x", None, &[file("pānui.pdf", b"%PDF")], None, None).unwrap());
        let m = MessageParser::default().parse(&raw).unwrap();
        assert_eq!(m.attachments().next().unwrap().attachment_name(), Some("pānui.pdf"));
    }

    #[test]
    fn argument_paths_must_be_absolute() {
        let e = resolve(&json!({"to": [addr("b")], "body": "x", "attachments": ["poster.pdf"]})).unwrap_err();
        assert!(e.to_string().contains("full path"), "{e}");
        let s = resolve(&json!({"body": "x", "attachments": ["/tmp/a/../poster.pdf"]})).unwrap();
        assert_eq!(s.attachments, vec![PathBuf::from("/tmp/poster.pdf")]);
        assert!(s.to.is_empty(), "a draft may have no recipients");
    }

    #[test]
    fn from_json_reads_paths_from_the_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        let build = root.join("talks/kōrero/build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(build.join("poster.pdf"), b"%PDF").unwrap();
        std::fs::write(build.join("poster.png"), b"png").unwrap();
        let email = json!({
            "to": "", "cc": [], "subject": "Talk on Friday",
            "body": "Hello", "body_html": "<p>Hello</p><!-- POSTER -->",
            "attachments": ["talks/kōrero/build/poster.pdf"],
            "poster_image": "talks/kōrero/build/poster.png",
            "poster_url": format!(" {} ", hosted("poster.png"))
        });
        std::fs::write(build.join("email.json"), email.to_string()).unwrap();
        let from_json = build.join("email.json").display().to_string();

        let to = format!("Someone <{}>", addr("c"));
        let s = resolve(&json!({"from_json": from_json, "to": [to]})).unwrap();
        assert_eq!(s.to, vec![to], "arguments fill in what the file leaves blank");
        assert_eq!(s.subject, "Talk on Friday");
        assert_eq!(s.attachments, vec![build.join("poster.pdf")]);
        assert_eq!(s.poster_image, Some(build.join("poster.png")));
        assert_eq!(s.poster_url, Some(hosted("poster.png")));
        let (files, poster) = load(&s).unwrap();
        assert_eq!(files[0].mime, "application/pdf");
        assert_eq!(poster.unwrap().mime, "image/png");
        assert_eq!(
            draft_paths(&json!({"from_json": from_json})),
            vec![
                from_json.clone(),
                build.join("poster.pdf").display().to_string(),
                build.join("poster.png").display().to_string()
            ]
        );

        // A path in the file may not leave its repository.
        let mut bad = email.clone();
        bad["attachments"] = json!(["../outside.pdf"]);
        std::fs::write(build.join("email.json"), bad.to_string()).unwrap();
        let e = resolve(&json!({"from_json": from_json})).unwrap_err();
        assert!(e.to_string().contains("outside its repository"), "{e}");
    }

    #[test]
    fn load_refuses_folders_and_non_image_posters() {
        let dir = tempfile::tempdir().unwrap();
        let s = DraftSpec {
            attachments: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        assert!(load(&s).unwrap_err().to_string().contains("not a file"));
        std::fs::write(dir.path().join("p.pdf"), b"%PDF").unwrap();
        let s = DraftSpec {
            poster_image: Some(dir.path().join("p.pdf")),
            ..Default::default()
        };
        assert!(load(&s).unwrap_err().to_string().contains("not an image"));
    }
}
