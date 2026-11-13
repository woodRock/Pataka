//! The text of a PDF, read on this computer, so it can be redacted like any
//! other text before a model sees it. Only the text layer is read: a scanned
//! PDF (pictures of pages) has none, and is refused rather than guessed at.
//! Images inside a PDF are never read or sent.

use std::time::Duration;

use crate::error::{Error, Result};

/// Larger PDFs are refused (they are rarely mostly text, and reading one
/// holds it all in memory).
pub const MAX_BYTES: usize = 40 * 1024 * 1024;
/// How long reading may take before Pātaka gives up on a PDF.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Letters and digits per page below which a PDF counts as having no text
/// layer: a scanned page, or one that is only pictures.
const MIN_CHARS_PER_PAGE: usize = 20;

/// A PDF's text, page by page.
#[derive(Debug, Clone, PartialEq)]
pub struct PdfText {
    pub pages: Vec<String>,
}

/// Read the text of `bytes`. Fails on anything that is not a readable PDF
/// with a text layer, saying why in words a person can act on.
pub fn extract(bytes: &[u8]) -> Result<PdfText> {
    if bytes.len() > MAX_BYTES {
        return Err(Error::Invalid(format!(
            "This PDF is {} MB; Pātaka reads PDFs up to {} MB.",
            bytes.len() / (1024 * 1024),
            MAX_BYTES / (1024 * 1024)
        )));
    }
    // The header may follow a little junk, as readers allow.
    let head = &bytes[..bytes.len().min(1024)];
    if !head.windows(5).any(|w| w == b"%PDF-") {
        return Err(Error::Invalid("This file is not a PDF.".into()));
    }
    let owned = bytes.to_vec();
    let (tx, rx) = std::sync::mpsc::channel();
    // The parser can panic or run long on a damaged file: run it on its own
    // thread, catch a panic, and stop waiting after the timeout.
    std::thread::Builder::new()
        .name("pdf-text".into())
        .spawn(move || {
            let r = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem_by_pages(&owned));
            let _ = tx.send(r);
        })
        .map_err(|_| Error::Invalid("Could not start reading the PDF.".into()))?;
    let pages = match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(Ok(pages))) => pages,
        Ok(Ok(Err(e))) => {
            let msg = e.to_string().to_lowercase();
            return Err(Error::Invalid(if msg.contains("encrypt") || msg.contains("password") {
                "This PDF is password-protected, so Pātaka cannot read it.".into()
            } else {
                "Pātaka could not read this PDF: it may be damaged.".into()
            }));
        }
        Ok(Err(_)) => {
            return Err(Error::Invalid(
                "Pātaka could not read this PDF: it may be damaged.".into(),
            ))
        }
        Err(_) => {
            return Err(Error::Invalid(
                "Reading this PDF took too long, so Pātaka stopped.".into(),
            ))
        }
    };
    let pages: Vec<String> = pages.iter().map(|p| tidy(p)).collect();
    let chars: usize = pages
        .iter()
        .map(|p| p.chars().filter(|c| c.is_alphanumeric()).count())
        .sum();
    if pages.is_empty() || chars < MIN_CHARS_PER_PAGE * pages.len().max(1) {
        return Err(Error::Invalid(
            "This PDF has no text to read: it is probably scanned, or only pictures. Pātaka does not read images yet, so nothing from it was sent."
                .into(),
        ));
    }
    Ok(PdfText { pages })
}

/// Control characters out, runs of blank lines and spaces shortened.
fn tidy(page: &str) -> String {
    let mut out = String::with_capacity(page.len());
    let mut blank = 0;
    for line in page.lines() {
        let line: String = line
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.trim().to_string()
}

/// Pages `a-b`, `a` or `a-` (1-based) of `total`, as a 0-based range.
pub fn page_range(spec: Option<&str>, total: usize) -> Result<std::ops::Range<usize>> {
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(0..total);
    };
    let bad = || {
        Error::Invalid(format!(
            "Pages are numbered 1 to {total}; ask for them as 3, 3-5 or 3-."
        ))
    };
    let num = |s: &str| s.trim().parse::<usize>().ok().filter(|n| (1..=total).contains(n));
    let (a, b) = match spec.split_once('-') {
        Some((a, b)) => (
            num(a).ok_or_else(bad)?,
            if b.trim().is_empty() {
                total
            } else {
                num(b).ok_or_else(bad)?
            },
        ),
        None => {
            let n = num(spec).ok_or_else(bad)?;
            (n, n)
        }
    };
    if a > b {
        return Err(bad());
    }
    Ok(a - 1..b)
}

impl PdfText {
    /// Pages `range` as text, each under a `[page N of M]` line, cut at
    /// about `cap` bytes with a note on which pages to ask for next.
    pub fn render(&self, range: std::ops::Range<usize>, cap: usize) -> String {
        let total = self.pages.len();
        let mut out = String::new();
        for i in range.clone() {
            let page = format!("[page {} of {total}]\n{}\n\n", i + 1, self.pages[i]);
            if !out.is_empty() && out.len() + page.len() > cap {
                out.push_str(&format!(
                    "[… pages {}-{} not shown: ask for pages {}- to read on]",
                    i + 1,
                    range.end,
                    i + 1
                ));
                return out;
            }
            if out.is_empty() && page.len() > cap {
                let mut end = cap;
                while !page.is_char_boundary(end) {
                    end -= 1;
                }
                out.push_str(&page[..end]);
                out.push_str(&format!(
                    "\n[… the rest of page {} was cut to keep the result short]",
                    i + 1
                ));
                if i + 1 < range.end {
                    out.push_str(&format!("\n[… ask for pages {}- to read on]", i + 2));
                }
                return out;
            }
            out.push_str(&page);
        }
        out.trim_end().to_string()
    }
}

/// A small PDF with one page per string, so tests need no files. Not
/// for anything else.
#[doc(hidden)]
pub fn sample(pages: &[&str]) -> Vec<u8> {
    let mut objs: Vec<String> = Vec::new();
    // 1: catalog, 2: pages, 3: font, then a page and its content each.
    let kids: Vec<String> = (0..pages.len()).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    objs.push("<< /Type /Catalog /Pages 2 0 R >>".into());
    objs.push(format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        pages.len()
    ));
    objs.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());
    for (i, text) in pages.iter().enumerate() {
        let mut stream = String::from("BT /F1 12 Tf 72 720 Td 14 TL\n");
        for line in text.lines() {
            let esc = line.replace('\\', "\\\\").replace('(', "\\(").replace(')', "\\)");
            stream.push_str(&format!("({esc}) Tj T*\n"));
        }
        stream.push_str("ET");
        objs.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            5 + 2 * i
        ));
        objs.push(format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()));
    }
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
    }
    let xref = out.len();
    out.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1));
    for o in offsets {
        out.push_str(&format!("{o:010} 00000 n \n"));
    }
    out.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objs.len() + 1
    ));
    out.into_bytes()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn pdf(pages: &[&str]) -> Vec<u8> {
        super::sample(pages)
    }

    #[test]
    fn reads_each_page() {
        let t = extract(&pdf(&[
            "Kia ora koutou, this is the first page.",
            "Second page: the method section.",
        ]))
        .unwrap();
        assert_eq!(t.pages.len(), 2);
        assert!(t.pages[0].contains("first page"), "{:?}", t.pages[0]);
        assert!(t.pages[1].contains("method section"), "{:?}", t.pages[1]);
        let r = t.render(0..2, 10_000);
        assert!(r.starts_with("[page 1 of 2]") && r.contains("[page 2 of 2]"));
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        assert!(extract(b"not a pdf").unwrap_err().to_string().contains("not a PDF"));
        let err = extract(&pdf(&[""])).unwrap_err().to_string();
        assert!(err.contains("no text"), "{err}");
        let mut damaged = pdf(&["Some text that is long enough to count as a page."]);
        damaged.truncate(60);
        assert!(extract(&damaged).is_err());
    }

    #[test]
    fn long_documents_are_cut_by_page() {
        let t = PdfText {
            pages: vec!["a".repeat(600), "b".repeat(600), "c".repeat(600)],
        };
        let r = t.render(0..3, 1000);
        assert!(r.contains("[page 1 of 3]") && !r.contains("[page 2 of 3]"));
        assert!(r.contains("ask for pages 2-"));
        let one = PdfText {
            pages: vec!["x".repeat(5000)],
        };
        assert!(one.render(0..1, 1000).contains("the rest of page 1 was cut"));
    }

    #[test]
    fn page_ranges() {
        assert_eq!(page_range(None, 5).unwrap(), 0..5);
        assert_eq!(page_range(Some("2-3"), 5).unwrap(), 1..3);
        assert_eq!(page_range(Some("4-"), 5).unwrap(), 3..5);
        assert_eq!(page_range(Some(" 5 "), 5).unwrap(), 4..5);
        assert!(page_range(Some("0"), 5).is_err());
        assert!(page_range(Some("4-2"), 5).is_err());
        assert!(page_range(Some("6"), 5).is_err());
    }
}

#[cfg(test)]
mod sample_files {
    /// Reads every PDF listed (one path per line) in `PATAKA_PDF_SAMPLES` and
    /// prints only counts, never the text: `cargo test -p pataka-core
    /// sample_pdfs -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn sample_pdfs() {
        let list = std::env::var("PATAKA_PDF_SAMPLES").expect("PATAKA_PDF_SAMPLES");
        for (n, path) in std::fs::read_to_string(list).unwrap().lines().enumerate() {
            let bytes = std::fs::read(path).unwrap();
            let start = std::time::Instant::now();
            match super::extract(&bytes) {
                Ok(t) => {
                    let chars: usize = t.pages.iter().map(|p| p.chars().count()).sum();
                    let words: usize = t.pages.iter().map(|p| p.split_whitespace().count()).sum();
                    let long = t
                        .pages
                        .iter()
                        .flat_map(|p| p.split_whitespace())
                        .filter(|w| w.chars().count() > 30)
                        .count();
                    println!(
                        "#{n}: {} KB, {} pages, {chars} chars, {words} words, {long} run-together words, {:?}",
                        bytes.len() / 1024,
                        t.pages.len(),
                        start.elapsed()
                    );
                }
                Err(e) => println!("#{n}: {} KB, refused: {e} ({:?})", bytes.len() / 1024, start.elapsed()),
            }
        }
    }
}
