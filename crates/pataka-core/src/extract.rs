//! The text of a file the person attaches to a kōrero, read on this
//! computer so it is redacted like anything else they type. Plain text and
//! code, PDF (its text layer), Word (.docx), PowerPoint (.pptx) and Excel
//! (.xlsx). Images and other binary files are refused, never sent.

use std::io::Read;

use crate::error::{Error, Result};

/// Larger files are refused.
pub const MAX_BYTES: usize = 20 * 1024 * 1024;
/// Text kept from one file. A message may hold 200,000 characters in all.
pub const MAX_CHARS: usize = 150_000;
/// An Office file's parts may not unpack to more than this (a zip bomb).
const MAX_UNPACKED: u64 = 64 * 1024 * 1024;

/// What was read from an attached file.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Extracted {
    pub name: String,
    /// "text", "pdf", "docx", "pptx" or "xlsx".
    pub kind: &'static str,
    pub text: String,
    pub bytes: usize,
    /// The text was cut at [`MAX_CHARS`].
    pub truncated: bool,
}

const IMAGE: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "heic", "heif", "bmp", "tif", "tiff", "svg", "ico", "avif",
];

/// Read the text of `bytes`, a file called `name`.
pub fn extract(name: &str, bytes: &[u8]) -> Result<Extracted> {
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name).trim().to_string();
    if bytes.len() > MAX_BYTES {
        return Err(Error::Invalid(format!(
            "{name} is {} MB; Pātaka reads attachments up to {} MB.",
            bytes.len() / (1024 * 1024),
            MAX_BYTES / (1024 * 1024)
        )));
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
    if IMAGE.contains(&ext.as_str()) {
        return Err(Error::Invalid(format!(
            "{name} is an image. Pātaka cannot attach images yet: it would have to check the picture itself for personal details."
        )));
    }
    let (kind, text) = match ext.as_str() {
        "pdf" => {
            let pdf = crate::pdf::extract(bytes).map_err(|e| Error::Invalid(format!("{name}: {e}")))?;
            let pages: Vec<String> = pdf
                .pages
                .iter()
                .enumerate()
                .map(|(i, p)| format!("[Page {}]\n{}", i + 1, p.trim()))
                .collect();
            ("pdf", pages.join("\n\n"))
        }
        "docx" | "docm" => ("docx", docx(&name, bytes)?),
        "pptx" | "pptm" => ("pptx", pptx(&name, bytes)?),
        "xlsx" | "xlsm" => ("xlsx", xlsx(&name, bytes)?),
        "doc" | "ppt" | "xls" | "odt" | "ods" | "odp" | "pages" | "numbers" | "key" | "rtfd" => {
            return Err(Error::Invalid(format!(
                "Pātaka cannot read {name}'s format. Save it as .docx, .xlsx, .pptx, PDF or plain text, and attach that."
            )))
        }
        _ => ("text", plain(&name, bytes)?),
    };
    let text = text.replace("\r\n", "\n");
    if text.trim().is_empty() {
        return Err(Error::Invalid(format!("{name} has no text to attach.")));
    }
    let truncated = text.chars().count() > MAX_CHARS;
    let text = if truncated {
        text.chars().take(MAX_CHARS).collect()
    } else {
        text
    };
    Ok(Extracted {
        name,
        kind,
        text,
        bytes: bytes.len(),
        truncated,
    })
}

/// Text in UTF-8 (with or without a byte-order mark) or UTF-16 with one.
fn plain(name: &str, bytes: &[u8]) -> Result<String> {
    let not_text = || {
        Error::Invalid(format!(
            "{name} is not a text file Pātaka can read. Attach text, PDF, Word, PowerPoint or Excel files."
        ))
    };
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8(rest.to_vec()).map_err(|_| not_text());
    }
    let utf16 = |rest: &[u8], le: bool| {
        if !rest.len().is_multiple_of(2) {
            return Err(not_text());
        }
        let units: Vec<u16> = rest
            .chunks(2)
            .map(|c| {
                if le {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .collect();
        String::from_utf16(&units).map_err(|_| not_text())
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, false);
    }
    if bytes.contains(&0) {
        return Err(not_text());
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| not_text())
}

/// Text kept from a file, in bytes, before it is cut to [`MAX_CHARS`]
/// characters (a character is at most four bytes).
const MAX_OUT: usize = MAX_CHARS * 4;

/// An Office file being read, with one budget for everything unpacked from
/// it, so a small file cannot unpack to gigabytes (a zip bomb).
struct Office<'a> {
    zip: zip::ZipArchive<std::io::Cursor<&'a [u8]>>,
    name: String,
    left: u64,
}

impl<'a> Office<'a> {
    fn open(name: &str, bytes: &'a [u8]) -> Result<Self> {
        let zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|_| {
            Error::Invalid(format!(
                "{name} could not be opened: it may be damaged or password-protected."
            ))
        })?;
        Ok(Office {
            zip,
            name: name.to_string(),
            left: MAX_UNPACKED,
        })
    }

    fn has(&self, path: &str) -> bool {
        self.zip.index_for_name(path).is_some()
    }

    fn names(&self) -> Vec<String> {
        self.zip.file_names().map(String::from).collect()
    }

    /// One part, as text. `None` if it is not there.
    fn part(&mut self, path: &str) -> Result<Option<String>> {
        let unreadable = || Error::Invalid(format!("{} could not be read.", self.name));
        let f = match self.zip.by_name(path) {
            Ok(f) => f,
            Err(zip::result::ZipError::FileNotFound) => return Ok(None),
            Err(_) => return Err(unreadable()),
        };
        let mut out = String::new();
        // One byte over the budget shows that it was exceeded.
        let n = f
            .take(self.left + 1)
            .read_to_string(&mut out)
            .map_err(|_| unreadable())?;
        if n as u64 > self.left {
            return Err(Error::Invalid(format!(
                "{} unpacks to more than {} MB, which Pātaka does not read.",
                self.name,
                MAX_UNPACKED / (1024 * 1024)
            )));
        }
        self.left -= n as u64;
        Ok(Some(out))
    }

    /// The parts a part refers to: relationship id → (type, full path).
    fn rels(&mut self, of: &str) -> Result<Vec<(String, String, String)>> {
        let (dir, file) = of.rsplit_once('/').unwrap_or(("", of));
        let Some(x) = self.part(&format!("{dir}/_rels/{file}.rels"))? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        walk(&x, |e| {
            if let Xml::Open("Relationship", a) = e {
                if let (Some(id), Some(target)) = (attr(a, "Id"), attr(a, "Target")) {
                    let ty = attr(a, "Type").unwrap_or("").to_string();
                    out.push((id.to_string(), ty, resolve(dir, &unescape(target))));
                }
            }
        });
        Ok(out)
    }
}

/// A relationship's target as a path in the archive.
fn resolve(dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Add `s` to `out` unless it already holds as much as is kept.
fn push(out: &mut String, s: &str) {
    if out.len() < MAX_OUT {
        out.push_str(s);
    }
}

/// A piece of Office XML: a tag (its name, and whether it closes) or text.
enum Xml<'a> {
    Open(&'a str, &'a str),
    Close(&'a str),
    Text(&'a str),
}

/// Walk `xml` tag by tag. Good enough for the text in Office files: no
/// DTDs, comments or CDATA are expected there.
fn walk<'a>(xml: &'a str, mut f: impl FnMut(Xml<'a>)) {
    let mut rest = xml;
    while !rest.is_empty() {
        match rest.find('<') {
            Some(0) => {
                let Some(end) = rest.find('>') else { break };
                let tag = &rest[1..end];
                rest = &rest[end + 1..];
                if tag.starts_with('?') || tag.starts_with('!') {
                    continue;
                }
                if let Some(n) = tag.strip_prefix('/') {
                    f(Xml::Close(n.trim()));
                } else {
                    let self_closing = tag.ends_with('/');
                    let tag = tag.trim_end_matches('/');
                    let (n, attrs) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
                    f(Xml::Open(n, attrs));
                    if self_closing {
                        f(Xml::Close(n));
                    }
                }
            }
            Some(i) => {
                f(Xml::Text(&rest[..i]));
                rest = &rest[i..];
            }
            None => {
                f(Xml::Text(rest));
                break;
            }
        }
    }
}

/// XML text with its entities decoded.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let c = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .or_else(|| ent.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// An attribute's value from a tag's attribute text.
fn attr<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = attrs;
    while let Some(i) = rest.find(key) {
        let before_ok = i == 0 || rest[..i].ends_with(char::is_whitespace);
        let after = &rest[i + key.len()..];
        if before_ok {
            if let Some(v) = after.trim_start().strip_prefix('=') {
                let v = v.trim_start();
                if let Some(q) = v.chars().next().filter(|q| *q == '"' || *q == '\'') {
                    let v = &v[1..];
                    return v.find(q).map(|e| &v[..e]);
                }
            }
        }
        rest = after;
    }
    None
}

/// Paragraph text from WordprocessingML or DrawingML: `text_tag` holds text,
/// `para_tag` ends a paragraph.
fn paragraphs(xml: &str, text_tag: &str, para_tag: &str) -> String {
    let mut out = String::new();
    let mut in_text = false;
    walk(xml, |x| match x {
        Xml::Open(n, _) if n == text_tag => in_text = true,
        Xml::Open("w:tab", _) => push(&mut out, "\t"),
        Xml::Open("w:br" | "w:cr" | "a:br", _) => push(&mut out, "\n"),
        Xml::Close(n) if n == text_tag => in_text = false,
        Xml::Close(n) if n == para_tag => push(&mut out, "\n"),
        Xml::Text(t) if in_text => push(&mut out, &unescape(t)),
        _ => {}
    });
    out
}

fn docx(name: &str, bytes: &[u8]) -> Result<String> {
    let mut o = Office::open(name, bytes)?;
    let body = o
        .part("word/document.xml")?
        .ok_or_else(|| Error::Invalid(format!("{name} is not a Word document.")))?;
    let mut out = paragraphs(&body, "w:t", "w:p");
    for extra in ["word/footnotes.xml", "word/endnotes.xml"] {
        if let Some(x) = o.part(extra)? {
            let t = paragraphs(&x, "w:t", "w:p");
            if !t.trim().is_empty() {
                push(&mut out, "\n[Notes]\n");
                push(&mut out, &t);
            }
        }
    }
    Ok(out)
}

/// The number in a part's file name, for ordering (`slide10` after `slide9`).
fn number(path: &str, prefix: &str) -> Option<u32> {
    path.strip_prefix(prefix)?.strip_suffix(".xml")?.parse().ok()
}

/// Parts named `prefix<N>.xml`, in number order: the fallback when a file
/// has no relationships saying the order.
fn numbered(o: &Office, prefix: &str) -> Vec<String> {
    let mut v: Vec<(u32, String)> = o
        .names()
        .into_iter()
        .filter_map(|p| number(&p, prefix).map(|n| (n, p)))
        .collect();
    v.sort();
    v.into_iter().map(|(_, p)| p).collect()
}

/// The parts `main` lists with `tag r:id="…"`, in its order, through its
/// relationships.
fn listed(o: &mut Office, main: &str, tag: &str) -> Result<Vec<(String, String)>> {
    let Some(x) = o.part(main)? else { return Ok(Vec::new()) };
    let rels = o.rels(main)?;
    let mut out = Vec::new();
    walk(&x, |e| {
        if let Xml::Open(n, a) = e {
            if n == tag {
                let target = attr(a, "r:id")
                    .and_then(|id| rels.iter().find(|r| r.0 == id))
                    .map(|r| r.2.clone());
                if let Some(t) = target {
                    out.push((attr(a, "name").map(unescape).unwrap_or_default(), t));
                }
            }
        }
    });
    Ok(out.into_iter().filter(|(_, p)| o.has(p)).collect())
}

fn pptx(name: &str, bytes: &[u8]) -> Result<String> {
    let mut o = Office::open(name, bytes)?;
    let mut slides: Vec<String> = listed(&mut o, "ppt/presentation.xml", "p:sldId")?
        .into_iter()
        .map(|(_, p)| p)
        .collect();
    if slides.is_empty() {
        slides = numbered(&o, "ppt/slides/slide");
    }
    if slides.is_empty() {
        return Err(Error::Invalid(format!("{name} is not a PowerPoint file.")));
    }
    let mut out = String::new();
    for (i, path) in slides.iter().enumerate() {
        if out.len() >= MAX_OUT {
            break;
        }
        let x = o.part(path)?.unwrap_or_default();
        push(&mut out, &format!("[Slide {}]\n", i + 1));
        push(&mut out, paragraphs(&x, "a:t", "a:p").trim());
        // The slide's own notes, found through its relationships.
        let notes = o
            .rels(path)?
            .into_iter()
            .find(|r| r.1.ends_with("/notesSlide"))
            .map(|r| r.2);
        if let Some(notes) = notes {
            if let Some(x) = o.part(&notes)? {
                let t = paragraphs(&x, "a:t", "a:p");
                let t = t.trim();
                if !t.is_empty() {
                    push(&mut out, "\n[Notes]\n");
                    push(&mut out, t);
                }
            }
        }
        push(&mut out, "\n\n");
    }
    Ok(out)
}

fn xlsx(name: &str, bytes: &[u8]) -> Result<String> {
    let mut o = Office::open(name, bytes)?;
    // Shared strings: <si> holds one string, in one or more <t>. Each is
    // kept to what a message could hold.
    let mut shared: Vec<String> = Vec::new();
    if let Some(x) = o.part("xl/sharedStrings.xml")? {
        let mut cur: Option<String> = None;
        let mut in_t = false;
        walk(&x, |e| match e {
            Xml::Open("si", _) => cur = Some(String::new()),
            Xml::Open("t", _) => in_t = true,
            Xml::Close("t") => in_t = false,
            Xml::Close("si") => shared.push(cur.take().unwrap_or_default()),
            Xml::Text(t) if in_t => {
                if let Some(c) = cur.as_mut() {
                    push(c, &unescape(t));
                }
            }
            _ => {}
        });
    }
    // Sheets in workbook order, with their names.
    let mut sheets = listed(&mut o, "xl/workbook.xml", "sheet")?;
    if sheets.is_empty() {
        sheets = numbered(&o, "xl/worksheets/sheet")
            .into_iter()
            .map(|p| (String::new(), p))
            .collect();
    }
    if sheets.is_empty() {
        return Err(Error::Invalid(format!("{name} is not an Excel workbook.")));
    }
    let mut out = String::new();
    for (i, (title, path)) in sheets.iter().enumerate() {
        if out.len() >= MAX_OUT {
            break;
        }
        let x = o.part(path)?.unwrap_or_default();
        let title = if title.is_empty() {
            format!("Sheet {}", i + 1)
        } else {
            title.clone()
        };
        push(&mut out, &format!("[Sheet: {title}]\n"));
        let mut row: Vec<String> = Vec::new();
        let mut row_len = 0usize;
        let mut cell_type = String::new();
        let mut cell = String::new();
        let mut in_value = false;
        walk(&x, |e| match e {
            Xml::Open("c", attrs) => {
                cell_type = attr(attrs, "t").unwrap_or("").to_string();
                cell.clear();
            }
            Xml::Open("v" | "t", _) => in_value = true,
            Xml::Close("v" | "t") => in_value = false,
            Xml::Text(t) if in_value => push(&mut cell, &unescape(t)),
            Xml::Close("c") => {
                // Nothing more is kept once the text is full.
                if out.len() + row_len >= MAX_OUT {
                    row.push(String::new());
                    return;
                }
                let v = if cell_type == "s" {
                    cell.trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|k| shared.get(k))
                        .map(|s| s.replace(['\t', '\n'], " "))
                        .unwrap_or_default()
                } else {
                    std::mem::take(&mut cell).replace(['\t', '\n'], " ")
                };
                row_len += v.len() + 1;
                row.push(v);
            }
            Xml::Close("row") => {
                while row.last().is_some_and(|c| c.is_empty()) {
                    row.pop();
                }
                if !row.is_empty() {
                    push(&mut out, &row.join("\t"));
                    push(&mut out, "\n");
                }
                row.clear();
                row_len = 0;
            }
            _ => {}
        });
        push(&mut out, "\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn office(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            for (p, x) in parts {
                z.start_file(*p, o).unwrap();
                z.write_all(x.as_bytes()).unwrap();
            }
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn plain_text_in_its_encodings() {
        assert_eq!(
            extract("notes.md", "Kia ora\r\nkōrero".as_bytes()).unwrap().text,
            "Kia ora\nkōrero"
        );
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend("a,b".as_bytes());
        assert_eq!(extract("t.csv", &bom).unwrap().text, "a,b");
        let mut le = vec![0xFF, 0xFE];
        for u in "hā".encode_utf16() {
            le.extend(u.to_le_bytes());
        }
        assert_eq!(extract("u16.txt", &le).unwrap().text, "hā");
        assert!(extract("x.bin", &[0, 1, 2, 3]).is_err());
        assert!(extract("y.txt", &[0xC3, 0x28]).is_err());
        assert!(extract("empty.txt", b"  \n").is_err());
        assert!(extract("/some/dir/a.txt", b"x").unwrap().name == "a.txt");
    }

    #[test]
    fn images_and_old_formats_are_refused() {
        let e = extract("photo.JPG", b"\xFF\xD8\xFF").unwrap_err().to_string();
        assert!(e.contains("image"), "{e}");
        let e = extract("old.doc", b"\xD0\xCF").unwrap_err().to_string();
        assert!(e.contains(".docx"), "{e}");
    }

    #[test]
    fn long_text_is_cut() {
        let big = "a".repeat(MAX_CHARS + 10);
        let x = extract("big.txt", big.as_bytes()).unwrap();
        assert!(x.truncated && x.text.chars().count() == MAX_CHARS);
    }

    #[test]
    fn word_documents() {
        let doc = r#"<?xml version="1.0"?><w:document><w:body>
            <w:p><w:r><w:t>Kia ora &amp; </w:t></w:r><w:r><w:t xml:space="preserve">welcome</w:t></w:r></w:p>
            <w:p><w:r><w:t>A</w:t><w:tab/><w:t>B&#x101;</w:t><w:br/><w:t>C</w:t></w:r></w:p>
            </w:body></w:document>"#;
        let x = extract("letter.docx", &office(&[("word/document.xml", doc)])).unwrap();
        assert_eq!(x.kind, "docx");
        assert_eq!(x.text.trim(), "Kia ora & welcome\nA\tBā\nC");
        assert!(extract("not.docx", &office(&[("x.xml", "<a/>")])).is_err());
        assert!(extract("broken.docx", b"PK not really").is_err());
    }

    fn rels(items: &[(&str, &str, &str)]) -> String {
        let r: String = items
            .iter()
            .map(|(id, ty, t)| format!(r#"<Relationship Id="{id}" Type="http://x/{ty}" Target="{t}"/>"#))
            .collect();
        format!("<Relationships>{r}</Relationships>")
    }

    #[test]
    fn slides_in_presentation_order_with_their_own_notes() {
        let slide = |t: &str| format!("<p:sld><a:p><a:r><a:t>{t}</a:t></a:r></a:p></p:sld>");
        let (s1, s2, s3) = (slide("Third"), slide("First"), slide("Second"));
        // The presentation lists slide2 first; slide2's notes are notesSlide5.
        let pres = r#"<p:presentation><p:sldIdLst><p:sldId id="1" r:id="rA"/><p:sldId id="2" r:id="rB"/><p:sldId id="3" r:id="rC"/></p:sldIdLst></p:presentation>"#;
        let pres_rels = rels(&[
            ("rA", "slide", "slides/slide2.xml"),
            ("rB", "slide", "slides/slide3.xml"),
            ("rC", "slide", "/ppt/slides/slide1.xml"),
        ]);
        let s2_rels = rels(&[("r1", "notesSlide", "../notesSlides/notesSlide5.xml")]);
        let notes = "<p:notes><a:p><a:r><a:t>Say hi</a:t></a:r></a:p></p:notes>";
        let x = extract(
            "talk.pptx",
            &office(&[
                ("ppt/presentation.xml", pres),
                ("ppt/_rels/presentation.xml.rels", &pres_rels),
                ("ppt/slides/slide1.xml", &s1),
                ("ppt/slides/slide2.xml", &s2),
                ("ppt/slides/slide3.xml", &s3),
                ("ppt/slides/_rels/slide2.xml.rels", &s2_rels),
                ("ppt/notesSlides/notesSlide5.xml", notes),
                (
                    "ppt/notesSlides/notesSlide1.xml",
                    "<p:notes><a:p><a:r><a:t>Wrong</a:t></a:r></a:p></p:notes>",
                ),
            ]),
        )
        .unwrap();
        assert_eq!(
            x.text.trim(),
            "[Slide 1]\nFirst\n[Notes]\nSay hi\n\n[Slide 2]\nSecond\n\n[Slide 3]\nThird"
        );
        // Without relationships, slides go by the number in their names.
        let x = extract(
            "bare.pptx",
            &office(&[
                ("ppt/slides/slide10.xml", &slide("Ten")),
                ("ppt/slides/slide9.xml", &slide("Nine")),
            ]),
        )
        .unwrap();
        assert_eq!(x.text.trim(), "[Slide 1]\nNine\n\n[Slide 2]\nTen");
    }

    #[test]
    fn workbooks_name_each_sheet_through_its_relationship() {
        let shared = r#"<sst><si><t>Name</t></si><si><r><t>Hē</t></r><r><t>mi</t></r></si></sst>"#;
        // The first sheet in the workbook is stored as sheet2.xml.
        let book = r#"<workbook><sheets><sheet name="Rōpū &amp; co" sheetId="1" r:id="rId9"/><sheet name="Other" sheetId="2" r:id="rId8"/></sheets></workbook>"#;
        let book_rels = rels(&[
            ("rId8", "worksheet", "worksheets/sheet1.xml"),
            ("rId9", "worksheet", "worksheets/sheet2.xml"),
        ]);
        let sheet = r#"<worksheet><sheetData>
            <row><c r="A1" t="s"><v>0</v></c><c r="B1"><v>Age</v></c></row>
            <row><c r="A2" t="s"><v>1</v></c><c r="B2"><v>42</v></c><c r="C2" t="inlineStr"><is><t>ok</t></is></c></row>
            </sheetData></worksheet>"#;
        let other = r#"<worksheet><sheetData><row><c><v>1</v></c></row></sheetData></worksheet>"#;
        let x = extract(
            "list.xlsx",
            &office(&[
                ("xl/sharedStrings.xml", shared),
                ("xl/workbook.xml", book),
                ("xl/_rels/workbook.xml.rels", &book_rels),
                ("xl/worksheets/sheet1.xml", other),
                ("xl/worksheets/sheet2.xml", sheet),
            ]),
        )
        .unwrap();
        assert_eq!(
            x.text.trim(),
            "[Sheet: Rōpū & co]\nName\tAge\nHēmi\t42\tok\n\n[Sheet: Other]\n1"
        );
    }

    #[test]
    fn a_zip_bomb_is_refused_or_cut_short() {
        // One shared string repeated in a great many cells.
        let big = "x".repeat(4 * 1024 * 1024);
        let shared = format!("<sst><si><t>{big}</t></si></sst>");
        let cells = r#"<c t="s"><v>0</v></c>"#.repeat(200_000);
        let sheet = format!("<worksheet><sheetData><row>{cells}</row></sheetData></worksheet>");
        let x = extract(
            "bomb.xlsx",
            &office(&[("xl/sharedStrings.xml", &shared), ("xl/worksheets/sheet1.xml", &sheet)]),
        )
        .unwrap();
        assert!(x.truncated && x.text.chars().count() == MAX_CHARS);
        // Parts that unpack past the budget all together are refused.
        let part = "y".repeat(40 * 1024 * 1024);
        let e = extract(
            "huge.docx",
            &office(&[("word/document.xml", &part), ("word/footnotes.xml", &part)]),
        )
        .unwrap_err();
        assert!(e.to_string().contains("unpacks to more than"), "{e}");
    }

    #[test]
    fn pdfs_page_by_page() {
        let pdf = crate::pdf::sample(&[
            "The first page of the paper has enough text to read",
            "Second page text, also long enough to count as text",
        ]);
        let x = extract("paper.pdf", &pdf).unwrap();
        assert_eq!(x.kind, "pdf");
        assert!(
            x.text.contains("[Page 1]") && x.text.contains("Second page"),
            "{}",
            x.text
        );
    }

    #[test]
    fn entities_and_attributes() {
        assert_eq!(unescape("a &lt;b&gt; &#233; &bogus; & x"), "a <b> é &bogus; & x");
        assert_eq!(attr(r#"r="A1" t="s""#, "t"), Some("s"));
        assert_eq!(attr(r#"rt="x" t='y'"#, "t"), Some("y"));
        assert_eq!(attr(r#"r="A1""#, "t"), None);
    }
}
