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
use tumataiti_rules::{Action, Category, Checksum, Source, Span};

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
    candidates.extend(model.iter().cloned());
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

    let allowed: Vec<String> = kaupapa
        .allow_list
        .iter()
        .chain(extra_allow)
        .map(|a| fold(a.trim()))
        .filter(|a| !a.is_empty())
        .collect();
    let mut spans = tumataiti_rules::merge(text, candidates);
    // A tapu span is never allowed through by an allow-list.
    spans.retain(|s| s.category == Category::CulturalRestricted || !allowed.contains(&fold(s.text.trim())));
    spans
}

/// The original values behind this kōrero's placeholders, as terms.
fn known_terms(vault: &Vault) -> Vec<Term> {
    vault
        .entities
        .iter()
        .filter_map(|(key, placeholder)| {
            let label = key.split('|').next()?;
            let category = Category::ALL.into_iter().find(|c| c.label() == label)?;
            let term = vault.placeholders.get(placeholder)?.clone();
            Some(Term { term, category })
        })
        .collect()
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
    // reply that repeats a name, a name in a different sentence).
    let mut terms = extra_terms.to_vec();
    terms.extend(known_terms(vault));
    let mut spans = detect(text, kaupapa, &terms, extra_allow, model);
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
