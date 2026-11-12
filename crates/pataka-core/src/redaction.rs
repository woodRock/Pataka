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
    match c {
        'ā' | 'Ā' => 'a',
        'ē' | 'Ē' => 'e',
        'ī' | 'Ī' => 'i',
        'ō' | 'Ō' => 'o',
        'ū' | 'Ū' => 'u',
        c => c.to_lowercase().next().unwrap_or(c),
    }
}

fn fold(s: &str) -> String {
    s.chars().map(fold_char).collect()
}

/// Entity key: case, spacing, punctuation and macrons ignored, so `Hēmi` and
/// `hemi` share a placeholder (as in Tūmataiti's own `entity_key`).
fn entity_key(category: Category, text: &str) -> String {
    let folded: String = text.chars().filter(|c| c.is_alphanumeric()).map(fold_char).collect();
    format!("{}|{}", category.label(), folded)
}

/// Every whole-word occurrence of `term` in `text`, ignoring case and macrons.
/// Returns UTF-8 byte ranges into `text`.
pub fn find_term(text: &str, term: &str) -> Vec<(usize, usize)> {
    let needle: Vec<char> = term.trim().chars().map(fold_char).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let chars: Vec<(usize, char)> = text.char_indices().map(|(i, c)| (i, fold_char(c))).collect();
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

/// Clean the name model's spans before they are trusted: drop fragments that
/// identify no one, take titles off names, and join a first and last name the
/// model returned as two spans, so a person gets one placeholder however the
/// model split them.
fn tidy_model_spans(text: &str, model: &[Span]) -> Vec<Span> {
    let mut spans: Vec<Span> = model
        .iter()
        .cloned()
        .map(|mut s| {
            if s.category == Category::Person {
                strip_honorifics(text, &mut s);
            }
            s
        })
        .filter(|s| substantial(s.category, &s.text))
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
    term_spans(text, &known_terms(vault), "korero_term", &mut known);
    // A name is caught again only where it is written as one: `Mark` the
    // person, not "mark as read".
    known.retain(|k| {
        !matches!(k.category, Category::Person | Category::Affiliation)
            || k.text.chars().next().is_some_and(char::is_uppercase)
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
            (substantial(category, &term) && alnum_count(&term) >= 3).then_some(Term { term, category })
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
    let mut spans = detect(text, kaupapa, extra_terms, extra_allow, model);
    fill_known(text, kaupapa, extra_allow, vault, &mut spans);
    widen_emails(text, &mut spans);
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
                        let n = vault.counters.entry(span.category).or_insert(0);
                        *n += 1;
                        let p = format!("[{}_{}]", span.category.label(), n);
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
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for f in &findings {
            if let Some(r) = &f.replacement {
                out.push_str(&text[cursor..f.start]);
                out.push_str(r);
                cursor = f.end;
            }
        }
        out.push_str(&text[cursor..]);
        out
    };
    Outcome {
        output,
        findings,
        blocked,
    }
}

/// Put original values back for display. Only placeholders this kōrero issued
/// are replaced, so a response cannot inject values of its own.
pub fn restore(text: &str, vault: &Vault) -> String {
    tumataiti_rules::restore(text, &vault.placeholders)
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
