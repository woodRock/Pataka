//! Tūmataiti integration.
//!
//! Detection uses the Tūmataiti rules (NZ identifiers with checksums, contact
//! details, secrets, labelled dates) plus the kaitiaki's gazetteer and tapu
//! terms. Overlaps are resolved by Tūmataiti's merge step. The policy is then
//! applied here rather than by `tumataiti_rules::redact::apply`, because a
//! kōrero spans many requests: the same person must keep the same placeholder
//! (`[PERSON_1]`) across every turn, so the placeholder vault is persistent.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
/// Re-exported so users of this crate need not depend on tumataiti-rules.
pub use tumataiti_rules::{Action, Category};
use tumataiti_rules::{Checksum, Source, Span};

use crate::model::{Kaupapa, Term};

/// Placeholder state for one kōrero. Stays on the device.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Vault {
    /// Placeholder → original text.
    pub placeholders: BTreeMap<String, String>,
    /// "CATEGORY|folded text" → placeholder.
    pub entities: BTreeMap<String, String>,
    pub counters: BTreeMap<Category, usize>,
    /// Authors of published work, seen where a bibliography lists them
    /// (`authors::bibliographic_names`). Their names are not hidden. Kept in
    /// memory only: the list is gone when the service locks or restarts, so
    /// a name is never public for longer than the work that showed it.
    #[serde(skip)]
    pub public_names: Vec<String>,
    /// The person's answer, while the service runs, to whether the authors
    /// a paper cites may go to the model (asked when the kaupapa says ask).
    #[serde(skip)]
    pub cited_answer: Option<bool>,
    /// Cited authors kept hidden until that question is answered.
    #[serde(skip)]
    pub cited_pending: Vec<String>,
}

impl Vault {
    /// Record the person's answer about cited authors: shown, the ones held
    /// back so far become public.
    pub fn answer_cited(&mut self, show: bool) {
        self.cited_answer = Some(show);
        let pending = std::mem::take(&mut self.cited_pending);
        if show {
            for n in pending {
                if !self.public_names.contains(&n) {
                    self.public_names.push(n);
                }
            }
        }
    }

    /// Whether cited authors are waiting on the person's answer.
    pub fn cited_question(&self) -> usize {
        if self.cited_answer.is_none() {
            self.cited_pending.len()
        } else {
            0
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub category: Category,
    pub label: &'static str,
    pub action: Action,
    pub replacement: Option<String>,
    pub detector: String,
    pub source: Source,
    pub confidence: f32,
    pub checksum: Checksum,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    /// What may be sent. Empty when blocked.
    pub output: String,
    pub findings: Vec<Finding>,
    pub blocked: bool,
}

/// Lowercase, macrons removed, keeping a map from each folded char back to
/// the byte range it came from in the original text.
fn fold_char(c: char) -> char {
    use unicode_normalization::UnicodeNormalization;
    // The letter without its accents (`ā`, `é` → `a`, `e`), lowercased.
    let base = std::iter::once(c).nfd().next().unwrap_or(c);
    base.to_lowercase().next().unwrap_or(base)
}

/// A combining mark: an accent or macron written as its own character after
/// the letter (decomposed text, as PDF tools often give). Folding ignores
/// it, and a span never stops just before one.
pub(crate) fn is_mark(c: char) -> bool {
    unicode_normalization::char::is_combining_mark(c)
}

fn fold(s: &str) -> String {
    s.chars().filter(|c| !is_mark(*c)).map(fold_char).collect()
}

/// Entity key: case, spacing, punctuation and macrons ignored, so `Hēmi` and
/// `hemi` share a placeholder (as in Tūmataiti's own `entity_key`).
fn entity_key(category: Category, text: &str) -> String {
    let folded: String = text
        .chars()
        .filter(|c| c.is_alphanumeric() && !is_mark(*c))
        .map(fold_char)
        .collect();
    format!("{}|{}", category.label(), folded)
}

/// Every whole-word occurrence of `term` in `text`, ignoring case and macrons.
/// Returns UTF-8 byte ranges into `text`.
pub fn find_term(text: &str, term: &str) -> Vec<(usize, usize)> {
    // Accents written as separate marks are skipped on both sides, so
    // decomposed text matches; a match ends after any marks that follow it
    // (the next letter's offset, or the end of the text).
    let needle: Vec<char> = term.trim().chars().filter(|c| !is_mark(*c)).map(fold_char).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let chars: Vec<(usize, char)> = text
        .char_indices()
        .filter(|(_, c)| !is_mark(*c))
        .map(|(i, c)| (i, fold_char(c)))
        .collect();
    let mut out = Vec::new();
    let n = needle.len();
    let mut i = 0;
    while i + n <= chars.len() {
        if chars[i..i + n].iter().map(|(_, c)| *c).eq(needle.iter().copied()) {
            let before_ok = i == 0 || !chars[i - 1].1.is_alphanumeric();
            let after_ok = i + n == chars.len() || !chars[i + n].1.is_alphanumeric();
            if before_ok && after_ok {
                let start = chars[i].0;
                let end = if i + n == chars.len() {
                    text.len()
                } else {
                    chars[i + n].0
                };
                out.push((start, end));
                i += n;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn term_spans(text: &str, terms: &[Term], detector: &str, out: &mut Vec<Span>) {
    for t in terms {
        for (start, end) in find_term(text, &t.term) {
            out.push(Span {
                start,
                end,
                text: text[start..end].to_string(),
                category: t.category,
                confidence: 1.0,
                source: Source::Gazetteer,
                detector: detector.to_string(),
                checksum: Checksum::None,
                note: None,
            });
        }
    }
}

fn alnum_count(s: &str) -> usize {
    s.chars().filter(|c| c.is_alphanumeric()).count()
}

/// Whether a value has enough to it to identify anyone. A lone digit or a
/// two-letter fragment does not, and hiding it would hide every "3" or "Te"
/// in the text. Names are written with a capital letter, so a lowercase word
/// taken for a person or an organisation is a common word, not a name.
fn substantial(category: Category, text: &str) -> bool {
    let letters = text.chars().filter(|c| c.is_alphabetic()).count();
    let n = alnum_count(text);
    if letters == 0 {
        return n >= 4;
    }
    if n < 2 {
        return false;
    }
    match category {
        Category::Person | Category::Affiliation => text.chars().any(char::is_uppercase),
        _ => true,
    }
}

/// Titles left outside a person's span, so `Dr Aroha Smith` and
/// `Aroha Smith` Share a placeholder.
const HONORIFICS: [&str; 12] = [
    "dr",
    "prof",
    "professor",
    "mr",
    "mrs",
    "ms",
    "miss",
    "mx",
    "sir",
    "dame",
    "assoc",
    "associate",
];

fn strip_honorifics(text: &str, span: &mut Span) {
    loop {
        let s = &text[span.start..span.end];
        let word_end = s.find(|c: char| !c.is_alphabetic()).unwrap_or(s.len());
        let word = s[..word_end].to_lowercase();
        if word_end == 0 || !HONORIFICS.contains(&word.as_str()) {
            return;
        }
        let rest = &s[word_end..];
        let rest_trimmed = rest.trim_start_matches('.').trim_start();
        if rest_trimmed.is_empty() {
            return;
        }
        span.start += s.len() - rest_trimmed.len();
        span.text = text[span.start..span.end].to_string();
    }
}

/// An email address in shape: something, `@`, and a domain with a dot in it.
/// `tool@1a2b3c4` (a model name with its revision) is not one.
fn looks_like_email(s: &str) -> bool {
    let Some((local, domain)) = s.trim().rsplit_once('@') else {
        return false;
    };
    let tld = domain.rsplit_once('.').map(|(_, t)| t).unwrap_or("");
    !local.is_empty() && tld.len() >= 2 && tld.chars().all(char::is_alphabetic)
}

/// A hex id: a commit hash, a content hash or a record id like `abd7e9a108`.
/// Lowercase hex with both digits and letters, 7 to 40 characters (a git
/// SHA at most: a longer hex string the model calls a secret stays hidden).
fn hex_id(s: &str) -> bool {
    (7..=40).contains(&s.len())
        && s.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().any(|c| c.is_ascii_alphabetic())
}

static CITATION_KEY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^[A-Za-z][A-Za-z'-]{0,40}:?(?:1[6-9]|20)\d{2}[A-Za-z0-9_-]{0,40}$").expect("pattern compiles")
});

/// A citation key like `kukutai2024science` or `Smith:2020a`: a word, a year
/// and maybe more. Needed to match claims to papers, and not secret.
fn citation_key(s: &str) -> bool {
    CITATION_KEY.is_match(s)
}

/// Ids and keys the name model takes for secrets, IDs or names. They identify
/// a record, not a person, and the agent must be able to type them back.
fn record_key(s: &str) -> bool {
    hex_id(s) || citation_key(s)
}

/// The word around `start..end`: letters, digits, `_` and `:`, so a model
/// span over part of `tmr2018principles` is judged by the whole key.
fn enclosing_word(text: &str, start: usize, end: usize) -> &str {
    if start > end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return "";
    }
    let part = |c: char| c.is_alphanumeric() || matches!(c, '_' | ':');
    let mut s = start;
    while let Some(c) = text[..s].chars().next_back().filter(|c| part(*c)) {
        s -= c.len_utf8();
    }
    let mut e = end;
    while let Some(c) = text[e..].chars().next().filter(|c| part(*c)) {
        e += c.len_utf8();
    }
    text[s..e].trim_matches(':')
}

/// Words for kin and collectives (whānau, hapū, iwi, koro…) and the small
/// words that join them. A span made only of these names no one: "te iwi"
/// or "ngā hapū" is a kind of group, not a particular one.
const COLLECTIVE_WORDS: [&str; 40] = [
    "whanau",
    "hapu",
    "iwi",
    "marae",
    "waka",
    "ropu",
    "tupuna",
    "tipuna",
    "mokopuna",
    "kaumatua",
    "kuia",
    "koro",
    "koroua",
    "nan",
    "nana",
    "papa",
    "mama",
    "whaea",
    "matua",
    "tamariki",
    "rangatahi",
    "rangatira",
    "tangata",
    "whenua",
    "tuakana",
    "teina",
    "tuahine",
    "tungane",
    "whanaunga",
    "uri",
    "kaitiaki",
    "taua",
    "poua",
    "pakeke",
    "te",
    "nga",
    "o",
    "a",
    "me",
    "ma",
];
const GLUE_WORDS: [&str; 6] = ["te", "nga", "o", "a", "me", "ma"];

fn only_collective_words(s: &str) -> bool {
    let words: Vec<String> = s
        .split(|c: char| !c.is_alphabetic() && !is_mark(c))
        .filter(|w| !w.is_empty())
        .map(fold)
        .collect();
    !words.is_empty()
        && words.iter().all(|w| COLLECTIVE_WORDS.contains(&w.as_str()))
        && words.iter().any(|w| !GLUE_WORDS.contains(&w.as_str()))
}

/// The run of letters, digits and hyphens around `start..end`: a span over
/// `DeBERTa` in `DeBERTa-v3-small` is judged by the whole model name.
fn hyphen_token(text: &str, start: usize, end: usize) -> &str {
    if start > end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return "";
    }
    let part = |c: char| c.is_alphanumeric() || is_mark(c) || matches!(c, '-' | '_' | '.');
    let mut a = start;
    while let Some(c) = text[..a].chars().next_back().filter(|c| part(*c)) {
        a -= c.len_utf8();
    }
    let mut b = end;
    while let Some(c) = text[b..].chars().next().filter(|c| part(*c)) {
        b += c.len_utf8();
    }
    &text[a..b]
}

/// Read's output numbers every line (`    55\tText`). Those numbers run
/// into the text and look like IDs, phone numbers or street numbers, so
/// detection reads them as spaces. Same length, so offsets still hold.
pub fn detection_view(text: &str) -> std::borrow::Cow<'_, str> {
    static NUMBERED: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?m)^ *(\d+)(?:\t|→)").expect("pattern compiles"));
    if NUMBERED.find_iter(text).take(2).count() < 2 {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = text.to_string();
    for c in NUMBERED.captures_iter(text) {
        let m = c.get(1).expect("group 1");
        out.replace_range(m.range(), &" ".repeat(m.len()));
    }
    std::borrow::Cow::Owned(out)
}

/// Finish spans so they leave nothing behind: over the accents written after
/// a letter, over the rest of a hyphenated name (`Rodriguez-Ruiz`, or the
/// `-effe` a line break split off), and over the next line's part of a URL
/// broken after `/`, `.` or `-`.
fn shape_spans(text: &str, spans: &mut Vec<Span>) {
    let url_char = |c: char| c.is_alphanumeric() || "-._~:/?#[]@!$&'*+,;=%".contains(c);
    let mut changed = false;
    for s in spans.iter_mut() {
        let (start, mut end) = (s.start, s.end);
        let mut start = start;
        if s.category == Category::Person {
            // The rest of a hyphenated name, on either side.
            loop {
                let rest = &text[end..];
                let mut it = rest.char_indices();
                match (it.next(), it.next()) {
                    // `Mont-` then a line break then `effe`: one word, split.
                    (Some((_, '-')), Some((_, '\n'))) if rest[2..].chars().next().is_some_and(char::is_lowercase) => {
                        end += 2;
                        while let Some(c) = text[end..].chars().next().filter(|c| c.is_alphabetic() || is_mark(*c)) {
                            end += c.len_utf8();
                        }
                    }
                    (Some((_, '-')), Some((_, c))) if c.is_alphabetic() => {
                        end += 1;
                        while let Some(c) = text[end..].chars().next().filter(|c| c.is_alphabetic() || is_mark(*c)) {
                            end += c.len_utf8();
                        }
                    }
                    _ => break,
                }
            }
            while text[..start].ends_with('-') && text[..start - 1].chars().next_back().is_some_and(char::is_alphabetic)
            {
                start -= 1;
                while let Some(c) = text[..start]
                    .chars()
                    .next_back()
                    .filter(|c| c.is_alphabetic() || is_mark(*c))
                {
                    start -= c.len_utf8();
                }
            }
        }
        if s.category == Category::Url && text[..end].ends_with(['/', '.', '-', '_', '=', '?', '&']) {
            let rest = &text[end..];
            let gap = rest.len() - rest.trim_start_matches([' ', '\t']).len();
            if rest[gap..].starts_with('\n') {
                let next = &rest[gap + 1..];
                let take: usize = next.chars().take_while(|c| url_char(*c)).map(char::len_utf8).sum();
                if take > 0 {
                    end += gap + 1 + take;
                }
            }
        }
        while let Some(c) = text[end..].chars().next().filter(|c| is_mark(*c)) {
            end += c.len_utf8();
        }
        if (start, end) != (s.start, s.end) {
            s.start = start;
            s.end = end;
            s.text = text[start..end].to_string();
            changed = true;
        }
    }
    if changed {
        *spans = tumataiti_rules::merge(text, std::mem::take(spans));
    }
}

/// Whether a model span is one to ignore: an "email" that is not an address,
/// or any span that falls on a hex id or citation key.
fn model_misfire(text: &str, s: &Span) -> bool {
    let name_like = matches!(s.category, Category::Person | Category::Affiliation);
    (s.category == Category::Email && !looks_like_email(&s.text))
        || record_key(enclosing_word(text, s.start, s.end))
        // A software or model name (`DeBERTa-v3-small`), not a person.
        || (name_like && hyphen_token(text, s.start, s.end).chars().any(|c| c.is_ascii_digit()))
        // "te iwi", "ngā hapū": a kind of group, not one. (A kinship word the
        // model takes for a person is flagged by Tūmataiti's review instead.)
        || (s.category == Category::Affiliation && only_collective_words(&s.text))
        // Chart labels and tables: an "IP address" that is not shaped like
        // one, or an "ID" that runs across lines or is a plain decimal.
        || (s.category == Category::IpAddress && !looks_like_ip(&s.text))
        // (A model NZ ID becomes an "ID" only after its check digits fail,
        // later, so it is judged here the same way.)
        || (matches!(s.category, Category::OtherId | Category::NzGovId)
            && (s.text.contains('\n') || plain_decimal(s.text.trim())))
}

/// An IPv4 address (four numbers of 0 to 255 joined by dots) or an IPv6 one
/// (hex groups joined by colons).
fn looks_like_ip(s: &str) -> bool {
    let s = s.trim();
    let v4 = s.split('.').count() == 4
        && s.split('.')
            .all(|p| !p.is_empty() && p.len() <= 3 && p.parse::<u16>().is_ok_and(|n| n <= 255));
    let v6 = s.contains(':') && s.split(':').count() >= 3 && s.chars().all(|c| c.is_ascii_hexdigit() || c == ':');
    v4 || v6
}

/// `21.83`, `0.4`, `95%`: a measurement, not an identifier.
fn plain_decimal(s: &str) -> bool {
    let s = s.trim_end_matches('%');
    let mut parts = s.split('.');
    let whole = parts.next().unwrap_or("");
    let frac = parts.next();
    parts.next().is_none()
        && !whole.is_empty()
        && whole.len() <= 6
        && whole.chars().all(|c| c.is_ascii_digit())
        && frac.is_some_and(|f| !f.is_empty() && f.chars().all(|c| c.is_ascii_digit()))
}

/// A name span that runs across a table's column gap (three or more spaces,
/// or a line break with indentation) into a cell with no letters in it (a
/// count, a year) is cut back to the cell with the name: in a table read as
/// text, `29⏎        Nemotron` is two cells, not one place. Cells with
/// letters on both sides of a gap are left together, so a name wrapped
/// over two lines stays one span.
fn trim_table_cells(text: &str, s: &mut Span) {
    if !matches!(s.category, Category::Person | Category::Address | Category::Affiliation) {
        return;
    }
    let inner = &text[s.start..s.end];
    // Cells: (start, end) within `inner`, split at gaps.
    let mut cells: Vec<(usize, usize)> = Vec::new();
    let mut cell_start = 0;
    let mut i = 0;
    let bytes = inner.as_bytes();
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            let run_end = i + bytes[i..].iter().take_while(|b| b.is_ascii_whitespace()).count();
            let run = &inner[i..run_end];
            let gap = run.len() >= 3 || (run.contains('\n') && run.len() >= 2);
            if gap {
                cells.push((cell_start, i));
                cell_start = run_end;
            }
            i = run_end;
        } else {
            i += 1;
        }
    }
    cells.push((cell_start, inner.len()));
    if cells.len() < 2 {
        return;
    }
    let lettered = |&(a, b): &(usize, usize)| inner[a..b].chars().any(char::is_alphabetic);
    let Some(first) = cells.iter().position(lettered) else {
        return;
    };
    let last = cells.iter().rposition(lettered).unwrap_or(first);
    let (start, end) = (s.start + cells[first].0, s.start + cells[last].1);
    if (start, end) != (s.start, s.end) {
        s.start = start;
        s.end = end;
        s.text = text[start..end].to_string();
    }
}

/// Clean the name model's spans before they are trusted: drop fragments that
/// identify no one, ids and keys it mistakes for secrets, take titles off
/// names, and join a first and last name the model returned as two spans, so
/// a person gets one placeholder however the model split them.
fn tidy_model_spans(text: &str, model: &[Span]) -> Vec<Span> {
    let mut spans: Vec<Span> = model
        .iter()
        .filter(|s| !model_misfire(text, s))
        .cloned()
        .map(|mut s| {
            if s.category == Category::Person {
                strip_honorifics(text, &mut s);
            }
            trim_table_cells(text, &mut s);
            s
        })
        .filter(|s| s.start < s.end && substantial(s.category, &s.text))
        .collect();
    spans.sort_by_key(|s| (s.start, s.end));
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for s in spans {
        if let Some(prev) = out.last_mut() {
            let joinable = prev.category == Category::Person
                && s.category == Category::Person
                && prev.end <= s.start
                && text[prev.end..s.start] == *" ";
            if joinable {
                prev.end = s.end;
                prev.text = text[prev.start..prev.end].to_string();
                prev.confidence = prev.confidence.min(s.confidence);
                continue;
            }
        }
        out.push(s);
    }
    out
}

fn rule_span(text: &str, start: usize, end: usize, category: Category, confidence: f32, detector: &str) -> Span {
    Span {
        start,
        end,
        text: text[start..end].to_string(),
        category,
        confidence,
        source: Source::Rule,
        detector: detector.to_string(),
        checksum: Checksum::None,
        note: None,
    }
}

static PERSONAL_SITE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)\b[a-z0-9][a-z0-9-]*\.(?:github\.io|gitlab\.io|netlify\.app|vercel\.app|pages\.dev|wordpress\.com|blogspot\.com|substack\.com|carrd\.co|wixsite\.com)\b(?:/[^\s<>()\[\]]*)?",
    )
    .expect("pattern compiles")
});

static STREET: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"\b\d{1,5}[A-Za-z]?,?\s+(?:[A-ZĀĒĪŌŪ][\p{L}'’-]+\s+){1,3}(?:Street|St|Road|Rd|Avenue|Ave|Terrace|Tce|Place|Pl|Crescent|Cres|Drive|Lane|Ln|Way|Quay|Highway|Hwy|Grove|Parade|Close|Court|Ct|Boulevard|Blvd|Esplanade|Square|Sq)\b\.?",
    )
    .expect("pattern compiles")
});

/// Rules Tūmataiti does not have yet: personal websites on hosting domains
/// (`name.github.io`), numbered street addresses, and the name part of an
/// email address where it appears on its own (`aroha.smith <…>`).
fn extra_rules(text: &str, found: &[Span], out: &mut Vec<Span>) {
    for m in PERSONAL_SITE.find_iter(text) {
        out.push(rule_span(
            text,
            m.start(),
            m.end(),
            Category::Url,
            0.95,
            "personal_site",
        ));
    }
    for m in STREET.find_iter(text) {
        out.push(rule_span(
            text,
            m.start(),
            m.end(),
            Category::Address,
            0.9,
            "street_address",
        ));
    }
    let emails: Vec<&Span> = found.iter().filter(|s| s.category == Category::Email).collect();
    let mut seen: Vec<String> = Vec::new();
    for e in &emails {
        let Some(local) = e.text.split('@').next() else {
            continue;
        };
        // Only name-like local parts: two or more letter groups joined by
        // `.`, `_` or `-` (`aroha.smith`), never `info` or `j123`.
        let parts: Vec<&str> = local.split(['.', '_', '-']).collect();
        let name_like = parts.len() >= 2 && parts.iter().all(|p| p.len() >= 2 && p.chars().all(char::is_alphabetic));
        if !name_like || seen.iter().any(|s| s.eq_ignore_ascii_case(local)) {
            continue;
        }
        seen.push(local.to_string());
        for (start, end) in find_term(text, local) {
            let inside_email = emails.iter().any(|x| start < x.end && x.start < end);
            let before_at = text[end..].starts_with('@');
            if !inside_email && !before_at {
                out.push(rule_span(text, start, end, Category::Person, 0.9, "email_name"));
            }
        }
    }
}

/// Detect spans: Tūmataiti rules, the student model's spans (if any), the
/// kaitiaki gazetteer, tapu terms and the kōrero's own terms. Model spans go
/// through Tūmataiti's review step (kinship words are flagged, IDs are
/// relabelled by their check digits), then everything is merged and anything
/// on an allow-list is removed.
pub fn detect(
    text: &str,
    kaupapa: &Kaupapa,
    extra_terms: &[Term],
    extra_allow: &[String],
    model: &[Span],
) -> Vec<Span> {
    let mut candidates = tumataiti_rules::rules::candidates(text);
    let mut extra = Vec::new();
    extra_rules(text, &candidates, &mut extra);
    candidates.extend(extra);
    candidates.extend(tidy_model_spans(text, model));
    tumataiti_rules::review::review(&mut candidates);
    term_spans(text, &kaupapa.gazetteer, "gazetteer", &mut candidates);
    term_spans(text, extra_terms, "korero_term", &mut candidates);
    let tapu: Vec<Term> = kaupapa
        .tapu_terms
        .iter()
        .map(|t| Term {
            term: t.clone(),
            category: Category::CulturalRestricted,
        })
        .collect();
    term_spans(text, &tapu, "tapu_term", &mut candidates);

    let allowed = allowed(kaupapa, extra_allow);
    let mut spans = tumataiti_rules::merge(text, candidates);
    // A tapu span is never allowed through by an allow-list.
    spans.retain(|s| s.category == Category::CulturalRestricted || !allowed.contains(&fold(s.text.trim())));
    spans
}

/// The kaitiaki's allow-list and the caller's, folded for comparison.
fn allowed(kaupapa: &Kaupapa, extra_allow: &[String]) -> Vec<String> {
    kaupapa
        .allow_list
        .iter()
        .chain(extra_allow)
        .map(|a| fold(a.trim()))
        .filter(|a| !a.is_empty())
        .collect()
}

/// Add spans for values this kōrero has already pseudonymised, but only where
/// nothing was detected this time. The vault catches a value the detectors
/// miss (a model reply that repeats a name), but it never overrides what they
/// find now: a value once labelled PERSON must not keep beating the model's
/// AFFILIATION, or the same text would be labelled differently depending on
/// how old the kōrero is.
fn fill_known(text: &str, kaupapa: &Kaupapa, extra_allow: &[String], vault: &Vault, spans: &mut Vec<Span>) {
    let mut known = Vec::new();
    term_spans(text, &known_terms(vault), "known_value", &mut known);
    // A name is caught again only where it is written as one: `Mark` the
    // person, not "mark as read".
    // Nor where it is joined by `_` into an identifier: `NAME_AGENT` is an
    // environment variable, not the person.
    known.retain(|k| {
        let glued = text[..k.start].ends_with('_') || text[k.end..].starts_with('_');
        !matches!(k.category, Category::Person | Category::Affiliation)
            || (k.text.chars().next().is_some_and(char::is_uppercase) && !glued)
    });
    if known.is_empty() {
        return;
    }
    // Only a detection that will replace the text outranks the vault. A weak
    // or allowed one would leave the value in the clear, where before it was
    // hidden.
    let policy = &kaupapa.policy;
    let replaces = |s: &Span| {
        s.category == Category::CulturalRestricted
            || (s.confidence >= policy.threshold && !matches!(policy.action(s.category), Action::Allow | Action::Flag))
    };
    let overlaps = |a: &Span, b: &Span| a.start < b.end && b.start < a.end;
    let allowed = allowed(kaupapa, extra_allow);
    let gaps: Vec<Span> = tumataiti_rules::merge(text, known)
        .into_iter()
        .filter(|k| !spans.iter().any(|s| replaces(s) && overlaps(s, k)))
        .filter(|k| !allowed.contains(&fold(k.text.trim())))
        .collect();
    if !gaps.is_empty() {
        spans.retain(|s| !gaps.iter().any(|k| overlaps(s, k)));
        spans.extend(gaps);
        spans.sort_by_key(|s| s.start);
    }
}

/// The original values behind this kōrero's placeholders, as terms. Values
/// too slight to identify anyone (an older model's "3" or "mark") are left
/// out, so one bad detection cannot hide that word for the rest of the
/// kōrero. Their placeholders still restore.
fn known_terms(vault: &Vault) -> Vec<Term> {
    vault
        .entities
        .iter()
        .filter_map(|(key, placeholder)| {
            let label = key.split('|').next()?;
            let category = Category::ALL.into_iter().find(|c| c.label() == label)?;
            let term = vault.placeholders.get(placeholder)?.clone();
            // An older model's misfires (a hex id, a citation key, a model
            // name taken for an email) are not caught again either. Nor are
            // kinship or group words alone (koro, te iwi), which name no one,
            // or a bare number the model took for an ID or IP address (a line
            // number, a chart label): the rules find real ones afresh, and
            // out of context a number is not worth hiding everywhere.
            let misfire = record_key(term.trim())
                || (category == Category::Email && !looks_like_email(&term))
                || (matches!(category, Category::Person | Category::Affiliation) && only_collective_words(&term))
                || (matches!(category, Category::OtherId | Category::IpAddress)
                    && !term.chars().any(char::is_alphabetic));
            (substantial(category, &term) && alnum_count(&term) >= 3 && !misfire).then_some(Term { term, category })
        })
        .collect()
}

/// A character that can be part of an email address.
fn email_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-' | '@')
}

/// Stretch every email span over the whole address around it, then merge
/// spans that now overlap. An address can be found in pieces (two values
/// from the vault, or two partial matches), and replacing each piece on its
/// own leaves the rest of the address in the clear between two placeholders.
/// Stretched, the address gets one placeholder.
fn widen_emails(text: &str, spans: &mut Vec<Span>) {
    let mut changed = false;
    for s in spans.iter_mut().filter(|s| s.category == Category::Email) {
        let mut start = s.start;
        while let Some(c) = text[..start].chars().next_back().filter(|c| email_char(*c)) {
            start -= c.len_utf8();
        }
        let mut end = s.end;
        while let Some(c) = text[end..].chars().next().filter(|c| email_char(*c)) {
            end += c.len_utf8();
        }
        // Sentence punctuation around an address is not part of it.
        while end > s.end && text[..end].ends_with(['.', '-']) {
            end -= 1;
        }
        while start < s.start && text[start..].starts_with(['.', '-']) {
            start += 1;
        }
        if (start, end) != (s.start, s.end) {
            s.start = start;
            s.end = end;
            s.text = text[start..end].to_string();
            changed = true;
        }
    }
    if changed {
        *spans = tumataiti_rules::merge(text, std::mem::take(spans));
    }
}

/// The person number in a person placeholder: 7 for both the first form
/// (`[PERSON_` `7]`) and a linked one (`[PERSON_` `7B]`).
fn person_number(placeholder: &str) -> Option<usize> {
    let rest = placeholder.strip_prefix('[')?.strip_suffix(']')?;
    let rest = rest.strip_prefix(Category::Person.label())?.strip_prefix('_')?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let tail = &rest[digits.len()..];
    (tail.is_empty() || (tail.len() == 1 && tail.chars().all(|c| c.is_ascii_uppercase())))
        .then(|| digits.parse().ok())
        .flatten()
}

/// A placeholder for another written form of a person this kōrero already
/// has: `Sweeney, L.` after `Latanya Sweeney` gets the same number with a
/// letter (`B`, `C`, …), so the model can tell it is the same name, while
/// each form still restores to exactly what was written. Only when every
/// earlier form it could be belongs to one person; two people who share a
/// surname leave the surname alone with a number of its own.
fn linked_placeholder(vault: &Vault, written: &str) -> Option<String> {
    let name = crate::authors::parse(written)?;
    let mut numbers: Vec<usize> = vault
        .placeholders
        .iter()
        .filter_map(|(p, original)| {
            let n = person_number(p)?;
            let other = crate::authors::parse(original)?;
            // A surname alone fits everyone with that surname, so it says
            // nothing about which of them a fuller form is.
            let says_who = name.given.is_empty() || !other.given.is_empty();
            (says_who && crate::authors::compatible(&name, &other)).then_some(n)
        })
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    let [n] = numbers[..] else {
        return None;
    };
    let label = Category::Person.label();
    ('B'..='Z')
        .map(|letter| format!("[{label}_{n}{letter}]"))
        .find(|p| !vault.placeholders.contains_key(p))
}

fn generalise(span: &Span) -> String {
    if span.category == Category::Date {
        let year = span
            .text
            .as_bytes()
            .windows(4)
            .position(|w| w.iter().all(u8::is_ascii_digit))
            .map(|i| &span.text[i..i + 4]);
        if let Some(year) = year {
            return format!("[YEAR {year}]");
        }
    }
    format!("[{}]", span.category.label())
}

/// Apply the policy to `text`, issuing placeholders from `vault` so that the
/// same entity keeps the same placeholder across the whole kōrero.
pub fn redact(
    text: &str,
    kaupapa: &Kaupapa,
    extra_terms: &[Term],
    extra_allow: &[String],
    model: &[Span],
    vault: &mut Vault,
) -> Outcome {
    // Anything this kōrero has already pseudonymised is caught again wherever
    // it appears, even where no detector would find it this time (a model
    // reply that repeats a name, a name in a different sentence), but what is
    // detected now decides the label.
    // Detection reads the text with Read's line numbers blanked; the output
    // is built from the text as given. The two have the same offsets.
    let output_text = text;
    let view = detection_view(text);
    let text: &str = &view;
    for name in crate::authors::bibliographic_names(text) {
        if !vault.public_names.contains(&name) {
            vault.public_names.push(name);
        }
    }
    let mut spans = detect(text, kaupapa, extra_terms, extra_allow, model);
    fill_known(text, kaupapa, extra_allow, vault, &mut spans);
    widen_emails(text, &mut spans);
    shape_spans(text, &mut spans);
    // Authors a paper cites, and the paper's own byline: shown, hidden or
    // held until the person says, as the kaupapa sets.
    let mut cited = crate::authors::cited_names(text);
    if let Some(end) = crate::authors::byline_end(text) {
        for s in spans.iter().filter(|s| s.category == Category::Person && s.end <= end) {
            if s.source != Source::Gazetteer || s.detector == "known_value" {
                cited.push(s.text.clone());
            }
        }
    }
    let show_cited = match kaupapa.cited_authors {
        crate::model::CitedAuthors::Show => true,
        crate::model::CitedAuthors::Hide => false,
        crate::model::CitedAuthors::Ask => vault.cited_answer == Some(true),
    };
    let ask = kaupapa.cited_authors == crate::model::CitedAuthors::Ask && vault.cited_answer.is_none();
    for name in cited {
        if show_cited {
            if !vault.public_names.contains(&name) {
                vault.public_names.push(name);
            }
        } else if ask && !vault.cited_pending.contains(&name) {
            vault.cited_pending.push(name);
        }
    }
    // Authors a bibliography has listed are public, in whatever form they
    // are written. A typed list or a gazetteer entry still wins: those are
    // the person's own choices, so only detected names are let through.
    // The model sometimes labels a cited name a place or a group (a first
    // author at the start of a reference, an organisation), so every label
    // a name can carry is checked.
    if !vault.public_names.is_empty() {
        spans.retain(|s| {
            !matches!(s.category, Category::Person | Category::Address | Category::Affiliation)
                || (s.source == Source::Gazetteer && s.detector != "known_value")
                || !crate::authors::is_public(&s.text, &vault.public_names)
        });
    }
    // A reference list is public metadata: when cited authors are shown, what
    // the model or the link rules find there (cities, publishers, title
    // words, page ranges, DOIs) is too. Contact details and the person's own
    // lists still apply.
    if show_cited {
        if let Some(refs) = crate::authors::references_start(text) {
            spans.retain(|s| {
                s.start < refs
                    || !(s.source == Source::Model || s.category == Category::Url || s.detector == "known_value")
            });
        }
    }
    // Placeholders already in the text (in a model's reply) stay as they are.
    let issued: Vec<(usize, usize)> = vault
        .placeholders
        .keys()
        .flat_map(|p| text.match_indices(p.as_str()).map(|(i, m)| (i, i + m.len())))
        .collect();
    spans.retain(|s| !issued.iter().any(|&(a, b)| s.start < b && a < s.end));
    let policy = &kaupapa.policy;
    let mut findings = Vec::with_capacity(spans.len());
    let mut blocked = false;

    for span in spans {
        let mut action = if span.confidence < policy.threshold {
            Action::Flag
        } else {
            policy.action(span.category)
        };
        // Tapu content is blocked whatever the policy table says.
        if span.category == Category::CulturalRestricted {
            action = Action::Block;
        }
        let replacement = match action {
            Action::Allow | Action::Flag => None,
            Action::Pseudonymise => {
                let key = entity_key(span.category, &span.text);
                let placeholder = match vault.entities.get(&key) {
                    Some(p) => p.clone(),
                    None => {
                        let linked = (span.category == Category::Person)
                            .then(|| linked_placeholder(vault, &span.text))
                            .flatten();
                        let p = linked.unwrap_or_else(|| {
                            let n = vault.counters.entry(span.category).or_insert(0);
                            *n += 1;
                            format!("[{}_{}]", span.category.label(), n)
                        });
                        vault.entities.insert(key, p.clone());
                        p
                    }
                };
                vault
                    .placeholders
                    .entry(placeholder.clone())
                    .or_insert_with(|| span.text.clone());
                Some(placeholder)
            }
            Action::Generalise => Some(generalise(&span)),
            Action::Mask => Some(format!("[{}]", span.category.label())),
            Action::Block => {
                blocked = true;
                Some(format!("[{}]", span.category.label()))
            }
        };
        findings.push(Finding {
            start: span.start,
            end: span.end,
            label: span.category.label(),
            text: span.text,
            category: span.category,
            action,
            replacement,
            detector: span.detector,
            source: span.source,
            confidence: span.confidence,
            checksum: span.checksum,
            note: span.note,
        });
    }

    let output = if blocked {
        String::new()
    } else {
        let mut out = String::with_capacity(output_text.len());
        let mut cursor = 0;
        for f in &findings {
            if let Some(r) = &f.replacement {
                out.push_str(&output_text[cursor..f.start]);
                out.push_str(r);
                cursor = f.end;
            }
        }
        out.push_str(&output_text[cursor..]);
        out
    };
    Outcome {
        output,
        findings,
        blocked,
    }
}

static PLACEHOLDER_LIKE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"\[((?:literal )*[A-Z][A-Z_]*_\d+[A-Z]?)\]").expect("pattern compiles")
});
static ESCAPED: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"\[literal ((?:literal )*[A-Z][A-Z_]*_\d+[A-Z]?)\]").expect("pattern compiles")
});

/// Mark text that already looks like a placeholder (a paper about
/// redaction, a test file) so it cannot be taken for one of Pātaka's: a
/// bracketed EMAIL_1 gains the word `literal` inside its brackets, and
/// `restore` takes it off again. Use on text from the person or their
/// files, never on the model's own words, whose placeholders are real.
pub fn escape_literals(text: &str) -> std::borrow::Cow<'_, str> {
    PLACEHOLDER_LIKE.replace_all(text, "[literal $1]")
}

/// Put original values back for display. Only placeholders this kōrero issued
/// are replaced, so a response cannot inject values of its own. Escaped
/// literals (`escape_literals`) become the text they were.
pub fn restore(text: &str, vault: &Vault) -> String {
    let restored = tumataiti_rules::restore(text, &vault.placeholders);
    ESCAPED.replace_all(&restored, "[$1]").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kaupapa() -> Kaupapa {
        Kaupapa::default()
    }

    #[test]
    fn email_pseudonymised_and_restored() {
        let mut v = Vault::default();
        let o = redact("Email hemi@example.co.nz please", &kaupapa(), &[], &[], &[], &mut v);
        assert!(!o.blocked);
        assert_eq!(o.output, "Email [EMAIL_1] please");
        assert_eq!(restore("Sent to [EMAIL_1].", &v), "Sent to hemi@example.co.nz.");
    }

    #[test]
    fn placeholders_persist_across_turns() {
        let mut v = Vault::default();
        let k = Kaupapa {
            gazetteer: vec![Term {
                term: "Hēmi Te Rangi".into(),
                category: Category::Person,
            }],
            ..kaupapa()
        };
        let a = redact("Kia ora, ko Hēmi Te Rangi tēnei.", &k, &[], &[], &[], &mut v);
        let b = redact(
            "Did hemi te rangi call? Also Aroha.",
            &k,
            &[Term {
                term: "Aroha".into(),
                category: Category::Person,
            }],
            &[],
            &[],
            &mut v,
        );
        assert_eq!(a.output, "Kia ora, ko [PERSON_1] tēnei.");
        assert_eq!(b.output, "Did [PERSON_1] call? Also [PERSON_2].");
        assert_eq!(restore("[PERSON_2] and [PERSON_1]", &v), "Aroha and Hēmi Te Rangi");
    }

    #[test]
    fn unknown_placeholders_are_left_alone() {
        let v = Vault::default();
        assert_eq!(restore("[PERSON_9] said hi", &v), "[PERSON_9] said hi");
    }

    #[test]
    fn tapu_term_blocks() {
        let mut v = Vault::default();
        let k = Kaupapa {
            tapu_terms: vec!["karakia tapu".into()],
            ..kaupapa()
        };
        let o = redact("Here is the Karakia Tapu text", &k, &[], &[], &[], &mut v);
        assert!(o.blocked);
        assert!(o.output.is_empty());
    }

    #[test]
    fn tapu_cannot_be_allow_listed() {
        let mut v = Vault::default();
        let k = Kaupapa {
            tapu_terms: vec!["wāhi tapu".into()],
            allow_list: vec!["wahi tapu".into()],
            ..kaupapa()
        };
        assert!(redact("the wāhi tapu site", &k, &[], &[], &[], &mut v).blocked);
    }

    #[test]
    fn secret_blocks() {
        let mut v = Vault::default();
        let o = redact(
            "my key is sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEF",
            &kaupapa(),
            &[],
            &[],
            &[],
            &mut v,
        );
        assert!(o.blocked);
    }

    #[test]
    fn ird_masked() {
        let mut v = Vault::default();
        let o = redact("My IRD number is 49-091-850", &kaupapa(), &[], &[], &[], &mut v);
        assert!(o.output.contains("[NZ_GOV_ID]"), "{}", o.output);
        assert!(!o.output.contains("49-091-850"));
    }

    #[test]
    fn allow_list_lets_tupuna_through() {
        let mut v = Vault::default();
        let k = Kaupapa {
            gazetteer: vec![Term {
                term: "Āpirana Ngata".into(),
                category: Category::Person,
            }],
            allow_list: vec!["Apirana Ngata".into()],
            ..kaupapa()
        };
        let o = redact("Āpirana Ngata wrote about this.", &k, &[], &[], &[], &mut v);
        assert_eq!(o.output, "Āpirana Ngata wrote about this.");
    }

    #[test]
    fn known_values_are_caught_again() {
        let mut v = Vault::default();
        let k = Kaupapa {
            gazetteer: vec![Term {
                term: "Jesse Wood".into(),
                category: Category::Person,
            }],
            ..kaupapa()
        };
        redact("Ko Jesse Wood tōku ingoa.", &k, &[], &[], &[], &mut v);
        // A later message, with no list entry and no model, still has it caught.
        let o = redact(
            "Kia ora Jesse Wood! [PERSON_1] is welcome.",
            &kaupapa(),
            &[],
            &[],
            &[],
            &mut v,
        );
        assert_eq!(o.output, "Kia ora [PERSON_1]! [PERSON_1] is welcome.");
    }

    #[test]
    fn placeholders_are_not_redacted_again() {
        let mut v = Vault::default();
        redact("Email hemi@example.co.nz", &kaupapa(), &[], &[], &[], &mut v);
        let text = "Reply to [EMAIL_1] today.";
        let start = text.find("[EMAIL_1]").unwrap();
        let model = [Span {
            start,
            end: start + "[EMAIL_1]".len(),
            text: "[EMAIL_1]".into(),
            category: Category::Person,
            confidence: 0.9,
            source: Source::Model,
            detector: "test".into(),
            checksum: Checksum::None,
            note: None,
        }];
        assert_eq!(redact(text, &kaupapa(), &[], &[], &model, &mut v).output, text);
    }

    #[test]
    fn whole_words_only() {
        assert_eq!(find_term("Mere and Meremere", "mere"), vec![(0, 4)]);
        assert_eq!(find_term("ko Ōtaki", "otaki"), vec![(3, 9)]);
    }
}

#[cfg(test)]
mod model_tests {
    use super::*;

    fn model_span(text: &str, needle: &str, category: Category, confidence: f32) -> Span {
        let start = text.find(needle).unwrap();
        Span {
            start,
            end: start + needle.len(),
            text: needle.into(),
            category,
            confidence,
            source: Source::Model,
            detector: "test-model".into(),
            checksum: Checksum::None,
            note: None,
        }
    }

    #[test]
    fn model_names_are_pseudonymised() {
        let text = "Ask Hēmi Te Rangi from Ngāti Porou.";
        let model = [
            model_span(text, "Hēmi Te Rangi", Category::Person, 0.98),
            model_span(text, "Ngāti Porou", Category::Affiliation, 0.95),
        ];
        let mut v = Vault::default();
        let o = redact(text, &Kaupapa::default(), &[], &[], &model, &mut v);
        assert_eq!(o.output, "Ask [PERSON_1] from [AFFILIATION_1].");
    }

    /// The first placeholder for `label`, built at run time.
    fn ph(label: &str) -> String {
        format!("[{label}_1]")
    }

    /// A vault where "Ōtaki" was once taken for a person.
    fn vault_with_person() -> Vault {
        let text = "Ko Ōtaki te kāinga.";
        let model = [model_span(text, "Ōtaki", Category::Person, 0.9)];
        let mut v = Vault::default();
        assert_eq!(
            redact(text, &Kaupapa::default(), &[], &[], &model, &mut v).output,
            format!("Ko {} te kāinga.", ph("PERSON"))
        );
        v
    }

    #[test]
    fn the_model_label_beats_an_old_vault_label() {
        let mut v = vault_with_person();
        let text = "E noho ana au ki Ōtaki.";
        let model = [model_span(text, "Ōtaki", Category::Affiliation, 0.95)];
        let o = redact(text, &Kaupapa::default(), &[], &[], &model, &mut v);
        assert_eq!(o.output, format!("E noho ana au ki {}.", ph("AFFILIATION")));
    }

    #[test]
    fn the_vault_still_catches_what_the_model_misses() {
        let mut v = vault_with_person();
        let o = redact("I haere au ki Ōtaki.", &Kaupapa::default(), &[], &[], &[], &mut v);
        assert_eq!(o.output, format!("I haere au ki {}.", ph("PERSON")));
    }

    #[test]
    fn a_weak_model_span_does_not_unhide_a_known_value() {
        let mut v = vault_with_person();
        let text = "I haere au ki Ōtaki.";
        let model = [model_span(text, "Ōtaki", Category::Affiliation, 0.05)];
        let o = redact(text, &Kaupapa::default(), &[], &[], &model, &mut v);
        assert_eq!(o.output, format!("I haere au ki {}.", ph("PERSON")));
    }

    #[test]
    fn kinship_word_alone_is_flagged_not_redacted() {
        let text = "Koro said hello.";
        let model = [model_span(text, "Koro", Category::Person, 0.9)];
        let mut v = Vault::default();
        let o = redact(text, &Kaupapa::default(), &[], &[], &model, &mut v);
        assert_eq!(o.output, text);
        assert_eq!(o.findings[0].action, Action::Flag);
    }

    #[test]
    fn allow_list_beats_the_model() {
        let text = "Āpirana Ngata wrote this.";
        let model = [model_span(text, "Āpirana Ngata", Category::Person, 0.99)];
        let k = Kaupapa {
            allow_list: vec!["Apirana Ngata".into()],
            ..Kaupapa::default()
        };
        let mut v = Vault::default();
        assert_eq!(redact(text, &k, &[], &[], &model, &mut v).output, text);
    }
}

#[cfg(test)]
mod precision_tests {
    use super::*;

    fn span(text: &str, needle: &str, category: Category) -> Span {
        let start = text.find(needle).unwrap();
        Span {
            start,
            end: start + needle.len(),
            text: needle.into(),
            category,
            confidence: 0.95,
            source: Source::Model,
            detector: "test-model".into(),
            checksum: Checksum::None,
            note: None,
        }
    }

    /// Placeholder `n` for `label`, built at run time.
    fn ph(label: &str, n: usize) -> String {
        format!("[{label}_{n}]")
    }

    fn run(text: &str, model: &[Span], v: &mut Vault) -> String {
        redact(text, &Kaupapa::default(), &[], &[], model, v).output
    }

    #[test]
    fn a_lone_digit_or_lowercase_word_is_not_hidden() {
        let text = "Level 3: mark it as read.";
        let model = [span(text, "3", Category::Address), span(text, "mark", Category::Person)];
        let mut v = Vault::default();
        assert_eq!(run(text, &model, &mut v), text);
        assert!(v.placeholders.is_empty());
    }

    #[test]
    fn slight_values_already_in_the_vault_are_not_caught_again() {
        let mut v = Vault::default();
        for (label, value) in [("ADDRESS", "3"), ("PERSON", "Mark"), ("PERSON", "Te")] {
            let p = ph(label, 1 + v.placeholders.len());
            v.placeholders.insert(p.clone(), value.into());
            v.entities.insert(
                entity_key(Category::ALL.into_iter().find(|c| c.label() == label).unwrap(), value),
                p,
            );
        }
        let text = "Trimester 3: mark as read. Te reo. Mark said yes.";
        let out = run(text, &[], &mut v);
        // "3" and "Te" are too slight to re-catch; "Mark" is caught only as a name.
        assert_eq!(
            out,
            format!("Trimester 3: mark as read. Te reo. {} said yes.", ph("PERSON", 2))
        );
        // Every placeholder still restores.
        assert_eq!(restore(&ph("ADDRESS", 1), &v), "3");
    }

    #[test]
    fn titles_do_not_change_the_placeholder() {
        let mut v = Vault::default();
        let a = "Dr Aroha Pōtiki wrote.";
        let b = "Aroha Pōtiki replied.";
        let out_a = run(a, &[span(a, "Dr Aroha Pōtiki", Category::Person)], &mut v);
        let out_b = run(b, &[span(b, "Aroha Pōtiki", Category::Person)], &mut v);
        assert_eq!(out_a, format!("Dr {} wrote.", ph("PERSON", 1)));
        assert_eq!(out_b, format!("{} replied.", ph("PERSON", 1)));
    }

    #[test]
    fn a_first_and_last_name_split_by_the_model_get_one_placeholder() {
        let mut v = Vault::default();
        let a = "Thanks Aroha Pōtiki.";
        let b = "From Aroha Pōtiki today.";
        run(a, &[span(a, "Aroha Pōtiki", Category::Person)], &mut v);
        let out = run(
            b,
            &[span(b, "Aroha", Category::Person), span(b, "Pōtiki", Category::Person)],
            &mut v,
        );
        assert_eq!(out, format!("From {} today.", ph("PERSON", 1)));
    }

    #[test]
    fn personal_sites_and_street_addresses_are_caught() {
        let mut v = Vault::default();
        let out = run(
            "See aroha-potiki.github.io or visit 297 Kauri Grove St, Nelson.",
            &[],
            &mut v,
        );
        assert!(!out.contains("github.io"), "site left in");
        assert!(!out.contains("Kauri"), "street left in");
        assert!(out.contains(", Nelson."));
    }

    #[test]
    fn an_address_found_in_pieces_is_hidden_whole() {
        let mut v = Vault::default();
        // The email rule stops at the second @, which would leave the end
        // of the address in the clear.
        let text = "Write to kiri@kowhai.nz.example.tui@ruru.net<https://example.org> today.";
        let out = run(text, &[], &mut v);
        assert!(
            !out.contains("ruru") && !out.contains("kowhai"),
            "part of the address left in"
        );
        assert!(out.starts_with("Write to [EMAIL_"), "unrelated text changed");
        assert_eq!(out.matches("[EMAIL_").count(), 1, "one placeholder for one address");

        // Two pieces with text between them, as the vault can find them.
        let text = "x kiri@kowhai.nz.example.tui@ruru.net y";
        let piece = |n: &str| {
            let start = text.find(n).unwrap();
            let mut s = span(text, n, Category::Email);
            s.start = start;
            s
        };
        let mut spans = vec![piece("kiri@kowhai.nz"), piece("tui@ruru")];
        widen_emails(text, &mut spans);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "kiri@kowhai.nz.example.tui@ruru.net");
    }

    #[test]
    fn sentence_punctuation_is_not_part_of_an_address() {
        let mut v = Vault::default();
        let out = run("Mail kiri@kowhai.nz.", &[], &mut v);
        assert!(
            out.starts_with("Mail [EMAIL_") && out.ends_with("]."),
            "full stop swallowed"
        );
    }

    #[test]
    fn the_name_part_of_an_email_is_caught_on_its_own() {
        let mut v = Vault::default();
        let text = "cc: aroha.potiki <aroha.potiki@example.org>, info <info@example.org>";
        let out = run(text, &[], &mut v);
        assert!(!out.contains("aroha"), "name part left in");
        assert!(
            out.contains("cc: ") && out.contains(", info <"),
            "unrelated text changed"
        );
    }
}

#[cfg(test)]
mod record_key_tests {
    use super::*;

    fn span(text: &str, needle: &str, category: Category) -> Span {
        let start = text.find(needle).unwrap();
        Span {
            start,
            end: start + needle.len(),
            text: needle.into(),
            category,
            confidence: 0.95,
            source: Source::Model,
            detector: "test-model".into(),
            checksum: Checksum::None,
            note: None,
        }
    }

    fn run(text: &str, model: &[Span], v: &mut Vault) -> String {
        redact(text, &Kaupapa::default(), &[], &[], model, v).output
    }

    #[test]
    fn hex_ids_are_not_taken_for_ids_or_secrets() {
        let text = "Claim abd7e9a108 cites commit 4b4a5d0.";
        let model = [
            span(text, "abd7e9a108", Category::OtherId),
            span(text, "4b4a5d0", Category::Secret),
        ];
        let mut v = Vault::default();
        assert_eq!(run(text, &model, &mut v), text);
    }

    #[test]
    fn citation_keys_are_left_whole() {
        let text = r#"{"keys": ["tmr2018principles"], "verdict": {"key": "tmr2018principles"}} Smith:2020a"#;
        let at = text.rfind("tmr2018principles").unwrap();
        let mut partial = span(text, "tmr2018", Category::Secret);
        partial.start = at;
        partial.end = at + "tmr2018".len();
        let model = [partial, span(text, "Smith:2020a", Category::Person)];
        let mut v = Vault::default();
        assert_eq!(run(text, &model, &mut v), text);
    }

    #[test]
    fn a_model_name_is_not_an_email() {
        let text = "Names found by tumataiti-student@ad7ac37.";
        let model = [span(text, "tumataiti-student@ad7ac37", Category::Email)];
        let mut v = Vault::default();
        assert_eq!(run(text, &model, &mut v), text);
        assert!(looks_like_email("someone@example.org"));
    }

    #[test]
    fn a_known_name_inside_an_env_var_is_left_alone() {
        let mut v = Vault::default();
        let first = "Ask Ruru first.";
        run(first, &[span(first, "Ruru", Category::Person)], &mut v);
        let out = run("Set RURU_AGENT=1, then ask Ruru.", &[], &mut v);
        assert!(out.starts_with("Set RURU_AGENT=1, then ask [PERSON_"), "{out}");
    }

    #[test]
    fn old_misfires_in_the_vault_are_not_caught_again() {
        let mut v = Vault::default();
        let text = "id abd7e9a108 by tumataiti-student@ad7ac37";
        run(
            text,
            &[
                span(text, "abd7e9a108", Category::Person),
                span(text, "tumataiti-student@ad7ac37", Category::Email),
            ],
            &mut v,
        );
        // Plant them as an older model would have.
        for (cat, value) in [
            (Category::OtherId, "abd7e9a108"),
            (Category::Email, "tumataiti-student@ad7ac37"),
        ] {
            let p = format!("[{}_9]", cat.label());
            v.placeholders.insert(p.clone(), value.into());
            v.entities.insert(entity_key(cat, value), p);
        }
        assert_eq!(run(text, &[], &mut v), text);
    }
}

#[cfg(test)]
mod name_form_tests {
    use super::*;

    fn span(text: &str, needle: &str) -> Span {
        let start = text.find(needle).unwrap();
        Span {
            start,
            end: start + needle.len(),
            text: needle.into(),
            category: Category::Person,
            confidence: 0.95,
            source: Source::Model,
            detector: "test-model".into(),
            checksum: Checksum::None,
            note: None,
        }
    }

    fn run(text: &str, names: &[&str], v: &mut Vault) -> String {
        let model: Vec<Span> = names.iter().map(|n| span(text, n)).collect();
        redact(text, &Kaupapa::default(), &[], &[], &model, v).output
    }

    /// Person placeholder `n` with an optional form letter, built at run time.
    fn ph(n: usize, letter: &str) -> String {
        format!("[{}_{n}{letter}]", Category::Person.label())
    }

    #[test]
    fn forms_of_one_name_share_a_number_and_restore_exactly() {
        let mut v = Vault::default();
        assert_eq!(
            run("By Aroha Kereama.", &["Aroha Kereama"], &mut v),
            format!("By {}.", ph(1, ""))
        );
        let text = "Kereama, A. and later Kereama alone.";
        let mut alone = span(text, "Kereama");
        alone.start = text.rfind("Kereama").unwrap();
        alone.end = alone.start + "Kereama".len();
        let out = redact(
            text,
            &Kaupapa::default(),
            &[],
            &[],
            &[span(text, "Kereama, A."), alone],
            &mut v,
        )
        .output;
        assert_eq!(out, format!("{} and later {} alone.", ph(1, "B"), ph(1, "C")));
        assert_eq!(restore(&out, &v), "Kereama, A. and later Kereama alone.");
        // Another person is a new number.
        assert_eq!(
            run("Then Mere Tipene.", &["Mere Tipene"], &mut v),
            format!("Then {}.", ph(2, ""))
        );
    }

    #[test]
    fn a_shared_surname_is_not_linked_to_either_person() {
        let mut v = Vault::default();
        run(
            "Aroha Kereama and Hemi Kereama.",
            &["Aroha Kereama", "Hemi Kereama"],
            &mut v,
        );
        let out = run("Ask Kereama.", &["Kereama"], &mut v);
        assert_eq!(out, format!("Ask {}.", ph(3, "")));
        // An initial still tells them apart.
        let out = run("Ask H. Kereama.", &["H. Kereama"], &mut v);
        assert_eq!(out, format!("Ask {}.", ph(2, "B")));
    }

    #[test]
    fn listed_authors_are_not_hidden_in_any_form() {
        let mut v = Vault::default();
        // Hidden before any bibliography lists her.
        let early = "Aroha Kereama wrote it.";
        assert!(!run(early, &["Aroha Kereama"], &mut v).contains("Aroha"));
        let json = r#"{"key": "kereama2020x", "authors": ["Aroha Kereama"]}"#;
        assert_eq!(run(json, &["Aroha Kereama"], &mut v), json);
        let byline = "KEREAMA, A. (2020). As Kereama argues, and Aroha Kereama wrote it.";
        assert_eq!(
            run(byline, &["KEREAMA, A.", "Kereama", "Aroha Kereama"], &mut v),
            byline
        );
        // Someone else with the surname is still hidden.
        assert!(!run("Hemi Kereama replied.", &["Hemi Kereama"], &mut v).contains("Hemi"));
        // Public names are not saved with the kōrero.
        let saved: Vault = serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
        assert!(saved.public_names.is_empty());
    }

    #[test]
    fn a_typed_person_list_still_wins_over_a_bibliography() {
        let mut v = Vault::default();
        let k = Kaupapa::default();
        let terms = [Term {
            term: "Aroha Kereama".into(),
            category: Category::Person,
        }];
        let text = "author = {Aroha Kereama}";
        let out = redact(text, &k, &terms, &[], &[], &mut v).output;
        assert!(!out.contains("Aroha"), "{out}");
    }

    #[test]
    fn person_numbers_read_both_forms() {
        assert_eq!(person_number(&ph(7, "")), Some(7));
        assert_eq!(person_number(&ph(12, "C")), Some(12));
        assert_eq!(person_number("[EMAIL_3]"), None);
        assert_eq!(person_number(&format!("[{}_7BB]", Category::Person.label())), None);
    }
}

#[cfg(test)]
mod cited_author_tests {
    use super::*;
    use crate::model::CitedAuthors;

    fn person(text: &str, needle: &str) -> Span {
        let start = text.find(needle).unwrap();
        Span {
            start,
            end: start + needle.len(),
            text: needle.into(),
            category: Category::Person,
            confidence: 0.95,
            source: Source::Model,
            detector: "test-model".into(),
            checksum: Checksum::None,
            note: None,
        }
    }

    fn kaupapa(c: CitedAuthors) -> Kaupapa {
        Kaupapa {
            cited_authors: c,
            ..Kaupapa::default()
        }
    }

    const PAPER: &str = "Re-identification\nAroha Kereama\nTe Herenga Waka\n\nAbstract\nAs Sweeney (2002) showed, and we thank Mere Tipene.\n\nReferences\nSweeney, L. (2002). k-Anonymity. IJUFKS.\n";

    fn spans() -> Vec<Span> {
        ["Aroha Kereama", "Sweeney", "Mere Tipene", "Sweeney, L."]
            .iter()
            .map(|n| person(PAPER, n))
            .collect()
    }

    #[test]
    fn shown_cited_authors_and_the_byline_go_through_but_thanked_people_do_not() {
        let mut v = Vault::default();
        let out = redact(PAPER, &kaupapa(CitedAuthors::Show), &[], &[], &spans(), &mut v).output;
        assert!(out.contains("Aroha Kereama") && out.contains("Sweeney (2002)"), "{out}");
        assert!(!out.contains("Tipene"), "acknowledged, not cited: {out}");
    }

    #[test]
    fn hidden_cited_authors_stay_placeholders_and_nothing_is_pending() {
        let mut v = Vault::default();
        let out = redact(PAPER, &kaupapa(CitedAuthors::Hide), &[], &[], &spans(), &mut v).output;
        assert!(!out.contains("Sweeney") && !out.contains("Kereama"), "{out}");
        assert_eq!(v.cited_question(), 0);
    }

    #[test]
    fn ask_holds_them_back_until_answered() {
        let k = kaupapa(CitedAuthors::Ask);
        let mut v = Vault::default();
        let out = redact(PAPER, &k, &[], &[], &spans(), &mut v).output;
        assert!(!out.contains("Sweeney"), "{out}");
        assert!(v.cited_question() > 0);
        v.answer_cited(true);
        assert_eq!(v.cited_question(), 0);
        let out = redact(PAPER, &k, &[], &[], &spans(), &mut v).output;
        assert!(out.contains("Aroha Kereama") && out.contains("Sweeney (2002)"), "{out}");
        assert!(!out.contains("Tipene"), "{out}");
        // A "no" is kept too: hidden, and no question.
        let mut v = Vault::default();
        redact(PAPER, &k, &[], &[], &spans(), &mut v);
        v.answer_cited(false);
        let out = redact(PAPER, &k, &[], &[], &spans(), &mut v).output;
        assert!(!out.contains("Sweeney"));
        assert_eq!(v.cited_question(), 0);
    }
}

#[cfg(test)]
mod paper_reading_tests {
    use super::*;
    use crate::model::CitedAuthors;

    fn span(text: &str, needle: &str, category: Category) -> Span {
        let start = text.find(needle).unwrap_or_else(|| panic!("{needle} not in text"));
        Span {
            start,
            end: start + needle.len(),
            text: needle.into(),
            category,
            confidence: 0.95,
            source: Source::Model,
            detector: "test-model".into(),
            checksum: Checksum::None,
            note: None,
        }
    }

    fn run(text: &str, model: &[Span], k: &Kaupapa, v: &mut Vault) -> String {
        redact(text, k, &[], &[], model, v).output
    }

    /// A placeholder-shaped token, built at run time.
    fn token(label: &str, n: usize) -> String {
        format!("[{label}_{n}]")
    }

    #[test]
    fn placeholder_shaped_text_is_escaped_and_restored_as_written() {
        let mut v = Vault::default();
        let mine = run(
            "Ask Aroha Kereama.",
            &[span("Ask Aroha Kereama.", "Aroha Kereama", Category::Person)],
            &Kaupapa::default(),
            &mut v,
        );
        assert!(mine.contains(&token("PERSON", 1)));
        // The paper writes the same token as an example.
        let paper = format!("Names become {} and {}.", token("PERSON", 1), token("EMAIL", 1));
        let escaped = escape_literals(&paper).into_owned();
        assert!(!escaped.contains(&token("PERSON", 1)), "{escaped}");
        let out = run(&escaped, &[], &Kaupapa::default(), &mut v);
        assert_eq!(
            restore(&out, &v),
            paper,
            "the paper's own token comes back, not the person's name"
        );
        // Escaping is undone exactly, even for text that was already escaped.
        let twice = escape_literals(&escaped).into_owned();
        assert_eq!(restore(&twice, &v), escaped);
    }

    #[test]
    fn decomposed_accents_stay_inside_the_span_and_match_lists() {
        let acute = '\u{301}';
        let macron = '\u{304}';
        let name = format!("Jose{acute} Pe{acute}rez");
        // The model's span stops before the accent on the final letter.
        let text = format!("Thanks to {name}{acute}.");
        let model = [span(&text, &name, Category::Person)];
        let mut v = Vault::default();
        let out = run(&text, &model, &Kaupapa::default(), &mut v);
        assert!(
            !out.contains(acute),
            "an accent was left outside the placeholder: {out:?}"
        );
        // A macron written as a mark still matches the allow-list and tapu terms.
        let pataka = format!("Pa{macron}taka");
        let text = format!("{pataka} hides names.");
        let model = [span(&text, &pataka, Category::Person)];
        let k = Kaupapa {
            allow_list: vec!["Pātaka".into()],
            ..Kaupapa::default()
        };
        assert_eq!(run(&text, &model, &k, &mut Vault::default()), text);
        let k = Kaupapa {
            tapu_terms: vec!["Pātaka".into()],
            ..Kaupapa::default()
        };
        assert!(redact(&text, &k, &[], &[], &[], &mut Vault::default()).blocked);
    }

    #[test]
    fn hyphenated_names_are_hidden_whole() {
        let text = "Work by Ana Rodriguez-Ruiz and de Mont-\neffe.";
        let model = [
            span(text, "Ruiz", Category::Person),
            span(text, "Mont", Category::Person),
        ];
        let out = run(text, &model, &Kaupapa::default(), &mut Vault::default());
        assert!(!out.contains("Rodriguez") && !out.contains("Ruiz"), "{out}");
        assert!(!out.contains("effe"), "{out}");
    }

    #[test]
    fn a_wrapped_url_is_hidden_with_its_tail() {
        let text = "See https://example.org/people/\naroha-kereama for more.";
        let out = run(text, &[], &Kaupapa::default(), &mut Vault::default());
        assert!(!out.contains("aroha"), "{out}");
        assert!(out.ends_with(" for more."), "{out}");
    }

    #[test]
    fn model_names_and_group_words_are_not_people_or_groups() {
        let text = "We fine-tune DeBERTa-v3-small with te iwi.";
        let model = [
            span(text, "DeBERTa", Category::Person),
            span(text, "te iwi", Category::Affiliation),
        ];
        assert_eq!(run(text, &model, &Kaupapa::default(), &mut Vault::default()), text);
    }

    #[test]
    fn read_line_numbers_are_not_read_as_values() {
        let text = "    55\tFirst line\n    56\tSecond line\n";
        let view = detection_view(text);
        assert_eq!(view.len(), text.len());
        assert!(!view.contains("55") && view.contains("First line"));
        assert_eq!(detection_view("Just 55 words."), "Just 55 words.");
    }

    #[test]
    fn cited_names_are_shown_under_any_label_and_the_reference_list_is_left_alone() {
        let k = Kaupapa {
            cited_authors: CitedAuthors::Show,
            ..Kaupapa::default()
        };
        let text = "Sovereignty matters (Te Mana Raraunga, 2018; Kereama, 2026), as de Montjoye et al. (2015) show.\n\nReferences\nKereama, A. (2026). Data in Florence. In Proc. ACL, Florence, pp. 4561–4570. https://doi.org/10.1/x\n";
        let model = [
            span(text, "Te Mana Raraunga", Category::Affiliation),
            span(text, "Kereama, 2026", Category::Address),
            span(text, "Montjoye", Category::Person),
            span(text, "Florence, pp", Category::Address),
            span(text, "4561", Category::OtherId),
        ];
        let mut model = model.to_vec();
        // The "Kereama" span covers the name only.
        model[1].end = model[1].start + "Kereama".len();
        model[1].text = "Kereama".into();
        model[3].end = model[3].start + "Florence".len();
        model[3].text = "Florence".into();
        let out = run(text, &model, &k, &mut Vault::default());
        assert_eq!(out, text);
        // Hidden, the same text keeps its placeholders.
        let k = Kaupapa {
            cited_authors: CitedAuthors::Hide,
            ..Kaupapa::default()
        };
        let out = run(text, &model, &k, &mut Vault::default());
        let body = out.split("References").next().unwrap_or_default();
        assert!(!body.contains("Kereama") && !body.contains("Raraunga") && !body.contains("Montjoye"));
    }
}

#[cfg(test)]
mod remembered_misfire_tests {
    use super::*;

    /// Remember `value` under `category`, as an older read would have.
    fn remember(v: &mut Vault, category: Category, value: &str, n: usize) -> String {
        let p = format!("[{}_{n}]", category.label());
        v.placeholders.insert(p.clone(), value.into());
        v.entities.insert(entity_key(category, value), p.clone());
        p
    }

    #[test]
    fn remembered_kinship_words_and_bare_numbers_are_not_caught_again() {
        let mut v = Vault::default();
        let koro = remember(&mut v, Category::Affiliation, "Koro", 1);
        remember(&mut v, Category::Affiliation, "te iwi", 2);
        let id = remember(&mut v, Category::OtherId, "21.83", 1);
        remember(&mut v, Category::IpAddress, "12.4.2", 1);
        let text = "Koro and te iwi scored 21.83 on 12.4.2.";
        let out = redact(text, &Kaupapa::default(), &[], &[], &[], &mut v).output;
        assert_eq!(out, text);
        // Their placeholders, already in a conversation, still restore.
        assert_eq!(restore(&format!("{koro} {id}"), &v), "Koro 21.83");
    }

    #[test]
    fn a_name_with_a_kinship_word_and_the_persons_own_list_still_hide() {
        let mut v = Vault::default();
        remember(&mut v, Category::Person, "Koro Wetere", 1);
        let out = redact("Koro Wetere spoke.", &Kaupapa::default(), &[], &[], &[], &mut v).output;
        assert!(!out.contains("Wetere"), "{out}");
        let terms = [Term {
            term: "Koro".into(),
            category: Category::Person,
        }];
        let out = redact(
            "Koro spoke.",
            &Kaupapa::default(),
            &terms,
            &[],
            &[],
            &mut Vault::default(),
        )
        .output;
        assert!(!out.starts_with("Koro"), "{out}");
    }
}

#[cfg(test)]
mod table_cell_tests {
    use super::*;

    fn model(text: &str, needle: &str, category: Category) -> Span {
        let start = text.find(needle).unwrap();
        Span {
            start,
            end: start + needle.len(),
            text: needle.into(),
            category,
            confidence: 0.95,
            source: Source::Model,
            detector: "test-model".into(),
            checksum: Checksum::None,
            note: None,
        }
    }

    #[test]
    fn a_count_in_the_next_table_cell_is_not_part_of_a_name() {
        let text = "Set                  Docs  Templates\nNZ dev               800          29\n   Pounamu-PII, train   20,000\n\n\
            References\nRangi, B. (2025). Pounamu-PII: Synthesized data for privacy.\n";
        let spans = [model(text, "29\n   Pounamu", Category::Address)];
        let mut vault = Vault::default();
        vault.answer_cited(true);
        let out = redact(text, &Default::default(), &[], &[], &spans, &mut vault).output;
        assert_eq!(out, text, "the count and the cited dataset are shown");
    }

    #[test]
    fn a_name_wrapped_over_two_lines_stays_one_span() {
        let text = "Next of kin: Hemi\n   Kerekere, ph 021 555 0101";
        let mut s = model(text, "Hemi\n   Kerekere", Category::Person);
        trim_table_cells(text, &mut s);
        assert_eq!(s.text, "Hemi\n   Kerekere");
        let mut s = model(text, "Hemi\n   Kerekere, ph 021", Category::Address);
        trim_table_cells(text, &mut s);
        assert_eq!(s.text, "Hemi\n   Kerekere, ph 021", "no gap before the number");
        let text = "Total      12      Hemi Kerekere";
        let mut s = model(text, "12      Hemi Kerekere", Category::Person);
        trim_table_cells(text, &mut s);
        assert_eq!(s.text, "Hemi Kerekere");
    }
}

#[cfg(test)]
mod chart_label_tests {
    use super::*;

    fn model(text: &str, needle: &str, category: Category) -> Span {
        let start = text.find(needle).unwrap();
        Span {
            start,
            end: start + needle.len(),
            text: needle.into(),
            category,
            confidence: 0.95,
            source: Source::Model,
            detector: "test-model".into(),
            checksum: Checksum::None,
            note: None,
        }
    }

    #[test]
    fn chart_numbers_are_not_ip_addresses_or_ids() {
        let text = "Leakage (%)\n12.3\n45.6\n78.9\n\n21.83\nserver 10.0.0.12 and passport X1234567";
        let spans = [
            model(text, "12.3\n45.6\n78.9", Category::IpAddress),
            model(text, "21.83", Category::OtherId),
            model(text, "45.6\n78.9", Category::OtherId),
            model(text, "78.9\n\n21", Category::NzGovId),
            model(text, "10.0.0.12", Category::IpAddress),
            model(text, "X1234567", Category::OtherId),
        ];
        let out = redact(text, &Kaupapa::default(), &[], &[], &spans, &mut Vault::default()).output;
        assert!(out.starts_with("Leakage (%)\n12.3\n45.6\n78.9\n\n21.83\n"), "{out}");
        assert!(
            !out.contains("10.0.0.12") && !out.contains("X1234567"),
            "real ones still hidden: {out}"
        );
    }
}
