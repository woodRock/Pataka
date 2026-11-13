//! Personal names as people write them: who the authors of a paper are, and
//! whether two written forms (`Latanya Sweeney`, `Sweeney, L.`, `SWEENEY`)
//! could be the same person.
//!
//! Authors of published work are public, and checking a paper against its
//! sources needs them, so names found where a bibliography lists authors
//! (a BibTeX `author` or `editor` field, an `authors` list in JSON or YAML)
//! are not hidden while the service runs. Names elsewhere are linked by
//! form, so the forms of one name share a placeholder number.

use std::sync::LazyLock;

use regex::Regex;

/// A name split into family name and given names, folded (lowercase, no
/// macrons) for comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name {
    pub family: String,
    /// Given names and initials, in order: `F. Jeffrey` is `["f", "jeffrey"]`.
    pub given: Vec<String>,
}

fn fold(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'ā' | 'Ā' => 'a',
            'ē' | 'Ē' => 'e',
            'ī' | 'Ī' => 'i',
            'ō' | 'Ō' => 'o',
            'ū' | 'Ū' => 'u',
            c => c,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// Lowercase words that belong to a family name (`de Montjoye`, `van der Berg`).
const PARTICLES: [&str; 10] = ["de", "del", "della", "der", "di", "du", "da", "van", "von", "le"];

/// Titles that are not part of a name.
const TITLES: [&str; 8] = ["dr", "prof", "professor", "mr", "mrs", "ms", "miss", "mx"];

/// Split a written name. `None` when it does not look like a name: no
/// letters, digits or `@` in it, or too many words.
pub fn parse(written: &str) -> Option<Name> {
    let s = written
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '’' | '`' | '(' | ')' | ',' | ';' | ':'));
    if s.is_empty() || s.len() > 100 || s.contains('@') || s.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let words = |t: &str| -> Vec<String> {
        t.split(|c: char| c.is_whitespace() || c == '.' || c == '-' || c == '‐')
            .map(|w| w.trim_matches(|c: char| !c.is_alphabetic() && c != '\'' && c != '’'))
            .filter(|w| !w.is_empty())
            .map(String::from)
            .collect()
    };
    let (family_words, given_words): (Vec<String>, Vec<String>) = if let Some((f, g)) = s.split_once(',') {
        (f.split_whitespace().map(String::from).collect(), words(g))
    } else {
        let mut tokens: Vec<String> = s.split_whitespace().map(String::from).collect();
        while tokens.len() > 1 && TITLES.contains(&fold(tokens[0].trim_end_matches('.')).as_str()) {
            tokens.remove(0);
        }
        let mut cut = tokens.len().saturating_sub(1);
        while cut > 0 && PARTICLES.contains(&tokens[cut - 1].as_str()) {
            cut -= 1;
        }
        let given = words(&tokens[..cut].join(" "));
        (tokens[cut..].to_vec(), given)
    };
    if family_words.is_empty() || family_words.len() > 4 || given_words.len() > 5 {
        return None;
    }
    let family = fold(
        &family_words
            .join(" ")
            .chars()
            .filter(|c| c.is_alphabetic() || matches!(c, ' ' | '-' | '\'' | '’'))
            .collect::<String>(),
    );
    if !family.chars().any(char::is_alphabetic) {
        return None;
    }
    Some(Name {
        family,
        given: given_words.iter().map(|g| fold(g)).collect(),
    })
}

/// Whether two names could be the same person: the same family name, and
/// given names that agree as far as both go (`l` agrees with `latanya`). A
/// form with no given names agrees with any.
pub fn compatible(a: &Name, b: &Name) -> bool {
    if a.family != b.family {
        return false;
    }
    a.given.iter().zip(&b.given).all(|(x, y)| {
        x == y
            || (x.chars().count() == 1 && y.starts_with(x.as_str()))
            || (y.chars().count() == 1 && x.starts_with(y.as_str()))
    })
}

static BIB_FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:author|editor)\s*=\s*([{\x22])").expect("pattern compiles"));
static JSON_AUTHORS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(?:authors|editors)"\s*:\s*\[([^\]]*)\]"#).expect("pattern compiles"));
static JSON_AUTHOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(?:author|editor)"\s*:\s*"((?:[^"\\]|\\.)*)""#).expect("pattern compiles"));
static JSON_STRING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""((?:[^"\\]|\\.)*)""#).expect("pattern compiles"));
static YAML_FLOW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[ \t]*(?:authors|editors):[ \t]*\[([^\]\n]*)\]").expect("pattern compiles"));
static YAML_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*(?:authors|editors):[ \t]*\n((?:[ \t]*-[ \t]+[^\n]+\n?)+)").expect("pattern compiles")
});
static AND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s+and\s+").expect("pattern compiles"));

/// The value of a BibTeX field starting just after its opening `{` or `"`.
fn bib_value(rest: &str, open: char) -> &str {
    if open == '"' {
        return rest.split('"').next().unwrap_or("");
    }
    let mut depth = 1;
    for (i, c) in rest.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &rest[..i];
                }
            }
            _ => {}
        }
    }
    rest
}

/// `{\'E}mile` → `Émile`-ish: braces and LaTeX accent commands removed.
fn untex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' | '}' | '~' => {
                if c == '~' {
                    out.push(' ');
                }
            }
            '\\' => {
                // An accent command (`\'`, `\"`, `\^`) or a letter command (`\c`).
                if let Some(&n) = chars.peek() {
                    if !n.is_alphanumeric() || chars.clone().nth(1).is_some_and(|m| m == '{' || m == ' ') {
                        chars.next();
                    }
                }
            }
            c => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Names that `text` lists as authors or editors of a work.
pub fn bibliographic_names(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        let n = name.trim().trim_matches(|c: char| c == '\'' || c == '"').trim();
        if parse(n).is_some() && !out.iter().any(|o| o == n) {
            out.push(n.to_string());
        }
    };
    for m in BIB_FIELD.captures_iter(text) {
        let open = m.get(1).expect("group 1").as_str().chars().next().unwrap_or('{');
        let value = untex(bib_value(&text[m.get(0).expect("match").end()..], open));
        AND.split(&value).for_each(&mut push);
    }
    for m in JSON_AUTHORS.captures_iter(text) {
        for s in JSON_STRING.captures_iter(&m[1]) {
            push(&s[1].replace("\\\"", "\""));
        }
    }
    for m in JSON_AUTHOR.captures_iter(text) {
        AND.split(&m[1]).for_each(&mut push);
    }
    for m in YAML_FLOW.captures_iter(text) {
        m[1].split(',').for_each(&mut push);
    }
    for m in YAML_BLOCK.captures_iter(text) {
        for line in m[1].lines() {
            push(line.trim().trim_start_matches('-'));
        }
    }
    out
}

/// Whether `written` names one of the `public` authors. A public surname
/// with no given names (from an in-text citation, "Sweeney (2002)") covers
/// only the surname written alone: it does not make every Sweeney public.
pub fn is_public(written: &str, public: &[String]) -> bool {
    let Some(name) = parse(written) else {
        return false;
    };
    public
        .iter()
        .filter_map(|p| parse(p))
        .any(|p| (!p.given.is_empty() || name.given.is_empty()) && compatible(&p, &name))
}

/// A surname as papers write it: capitalised, perhaps with a particle
/// (`de Montjoye`), a hyphen or an apostrophe.
const SURNAME: &str = r"(?:(?:de|van|von|der|di|da|du|le)\s)?\p{Lu}[\p{L}'’-]+";
/// A year in a citation: 1600 to 2099, perhaps with a letter (`2019a`).
const YEAR: &str = r"(?:1[6-9]|20)\d{2}[a-z]?";

/// Words that sit where a cited surname would but are not one.
const NOT_SURNAMES: [&str; 24] = [
    "act", "and", "appendix", "chapter", "eq", "equation", "fig", "figure", "in", "january", "june", "july", "march",
    "may", "note", "part", "section", "see", "since", "table", "the", "version", "volume", "year",
];

static PAREN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(([^()]{4,400})\)").expect("pattern compiles"));
static PAREN_PART: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"^\s*(?:(?:see|e\.g\.|cf\.|i\.e\.)[,\s]+)*((?:{SURNAME})(?:(?:,\s*|\s+(?:and|&)\s+)(?:{SURNAME}))*(?:,?\s+et\s+al\.?)?),?\s+{YEAR}(?:,\s*(?:p|pp|ch)\.?\s*[\d–-]+)?\s*$"
    ))
    .expect("pattern compiles")
});
static NARRATIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"((?:{SURNAME})(?:(?:,\s*|\s+(?:and|&)\s+)(?:{SURNAME}))*)(?:\s+et\s+al\.?)?\s+(?:\({YEAR}\)|\[\d+(?:[,–-]\s*\d+)*\])"
    ))
    .expect("pattern compiles")
});
static REFERENCES_HEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?mi)^\s*(?:\d+\.?\s*|[ivx]+\.\s*)?(?:references|bibliography|works cited|literature cited|reference list|ngā tohutoro)\s*$")
        .expect("pattern compiles")
});
static ABSTRACT_HEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?mi)^\s*(?:abstract|summary|tuhinga whakarāpopoto)\b[.:—-]?").expect("pattern compiles")
});
static SURNAME_INITIALS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"({SURNAME}(?:\s\p{{Lu}}[\p{{L}}'’-]+)?),\s((?:\p{{Lu}}\.(?:\s?-?\s?\p{{Lu}}\.)*\s?){{1,4}})"
    ))
    .expect("pattern compiles")
});
static INITIALS_SURNAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?:^|[\s,\]])((?:\p{{Lu}}\.\s?-?){{1,4}})\s?({SURNAME})")).expect("pattern compiles")
});
static FULL_NAME_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?m)^\s*(?:\[\d+\]\s*)?([^.\[\]\n“"]{{3,300}}?)(?:\.\s+{YEAR}\.|,\s*[“"])"#
    ))
    .expect("pattern compiles")
});
static FULL_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\p{Lu}[\p{L}'’.-]*(?:\s+(?:(?:de|van|von|der|di|da|du|le)\s+)?\p{Lu}[\p{L}'’.-]*){1,3}$")
        .expect("pattern compiles")
});

/// Surnames in a list of cited names: `Rocher, Hendrickx and de Montjoye et al.`.
fn split_surnames(names: &str) -> Vec<String> {
    let cut = names.trim().trim_end_matches('.');
    let cut = cut.strip_suffix("et al").unwrap_or(cut);
    AND.split(cut)
        .flat_map(|p| p.split([',', '&']))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty() && !NOT_SURNAMES.contains(&fold(p).as_str()))
        .collect()
}

/// Where a paper's byline ends: the start of an Abstract heading near the
/// top. Names before it are the paper's authors. `None` when the text does
/// not open like a paper.
pub fn byline_end(text: &str) -> Option<usize> {
    let head = &text[..text.len().min(3000)];
    let head = &head[..(0..=head.len()).rev().find(|&i| head.is_char_boundary(i)).unwrap_or(0)];
    ABSTRACT_HEADING.find(head).map(|m| m.start()).filter(|&i| i > 0)
}

/// Names `text` cites as authors of published work: in a reference list
/// (after a References heading) every author of every entry, and anywhere
/// the surnames of an in-text citation, which come with a year or a
/// reference number (`(Sweeney, 2002)`, `Rocher et al. [12]`). Names in
/// prose, acknowledgements or quotes are not citations and are not found.
pub fn cited_names(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |name: String| {
        let n = name.trim().trim_end_matches(',').trim().to_string();
        if parse(&n).is_some() && !NOT_SURNAMES.contains(&fold(&n).as_str()) && !out.contains(&n) {
            out.push(n);
        }
    };
    for m in PAREN.captures_iter(text) {
        for part in m[1].split(';') {
            if let Some(c) = PAREN_PART.captures(part) {
                split_surnames(&c[1]).into_iter().for_each(&mut push);
            }
        }
    }
    for c in NARRATIVE.captures_iter(text) {
        split_surnames(&c[1]).into_iter().for_each(&mut push);
    }
    if let Some(h) = REFERENCES_HEADING.find_iter(text).last() {
        let refs = &text[h.end()..];
        let joined = refs.replace('\n', " ");
        for c in SURNAME_INITIALS.captures_iter(&joined) {
            push(format!("{}, {}", &c[1], c[2].trim()));
        }
        for c in INITIALS_SURNAME.captures_iter(&joined) {
            push(format!("{} {}", c[1].trim(), &c[2]));
        }
        for c in FULL_NAME_BLOCK.captures_iter(refs) {
            for piece in AND.split(&c[1]).flat_map(|p| p.split(',')) {
                let piece = piece.trim();
                if FULL_NAME.is_match(piece) {
                    push(piece.to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Name {
        parse(s).unwrap_or_else(|| panic!("{s} did not parse"))
    }

    #[test]
    fn forms_of_one_name_are_compatible() {
        let full = n("Latanya Sweeney");
        for form in [
            "Sweeney, Latanya",
            "L. Sweeney",
            "Sweeney, L.",
            "SWEENEY",
            "Sweeney",
            "Dr Latanya Sweeney",
        ] {
            assert!(compatible(&full, &n(form)), "{form}");
        }
        assert!(!compatible(&full, &n("Mary Sweeney")));
        assert!(!compatible(&full, &n("Latanya Smith")));
        assert!(compatible(&n("F. Jeffrey Friedlin"), &n("Friedlin, F.")));
        assert_eq!(n("Yves-Alexandre de Montjoye").family, "de montjoye");
        assert!(compatible(&n("de Montjoye, Yves-Alexandre"), &n("Y.-A. de Montjoye")));
        assert!(parse("tmr2018principles").is_none());
        assert!(parse("someone@example.org").is_none());
    }

    #[test]
    fn authors_are_found_in_bib_json_and_yaml() {
        let bib = "@article{x,\n  author    = {Hui Yang and Jonathan M. Garibaldi},\n  editor = \"Sm{\\'i}th, Ana\",\n  title = {About Someone Else}\n}";
        assert_eq!(
            bibliographic_names(bib),
            ["Hui Yang", "Jonathan M. Garibaldi", "Smith, Ana"]
        );
        let json =
            r#"{"key": "sweeney2002kanonymity", "authors": ["Latanya Sweeney", "Luc Rocher"], "title": "Not A Name"}"#;
        assert_eq!(bibliographic_names(json), ["Latanya Sweeney", "Luc Rocher"]);
        let yaml = "---\nkey: x\nauthors: [Latanya Sweeney, 'Mere Te Huia']\nyear: 2002\n";
        assert_eq!(bibliographic_names(yaml), ["Latanya Sweeney", "Mere Te Huia"]);
        let block = "authors:\n  - Ana Smith\n  - Bo Li\ntitle: x\n";
        assert_eq!(bibliographic_names(block), ["Ana Smith", "Bo Li"]);
        // Prose that mentions authors is not a list of them.
        assert!(bibliographic_names("The authors thank Ana Smith.").is_empty());
    }

    #[test]
    fn in_text_citations_give_surnames_only() {
        let text = "Quasi-identifiers re-identify people (Sweeney, 2002; Rocher et al., 2019). \
            As Narayanan and Shmatikov (2008) showed, and Ohm [7] argued, de Montjoye et al. [3, 4] agree. \
            See Table 2 (2019). We thank Aroha Kereama (pers. comm.) for comments.";
        let names = cited_names(text);
        for n in ["Sweeney", "Rocher", "Narayanan", "Shmatikov", "Ohm", "de Montjoye"] {
            assert!(names.iter().any(|x| x == n), "{n} missing from {names:?}");
        }
        assert!(
            !names.iter().any(|x| x.contains("Kereama") || x == "Table"),
            "{names:?}"
        );
    }

    #[test]
    fn reference_lists_give_whole_names() {
        let text = "Body text by Mere Tipene.\n\nReferences\n\
            Sweeney, L. (2002). k-Anonymity: A model for protecting privacy. IJUFKS, 10(5).\n\
            [12] L. Rocher, J. M. Hendrickx, and Y.-A. de Montjoye, “Estimating the success of re-identifications,” Nat. Commun., 2019.\n\
            Arvind Narayanan and Vitaly Shmatikov. 2008. Robust de-anonymization of large sparse datasets. In S&P.\n";
        let names = cited_names(text);
        for n in [
            "Sweeney, L.",
            "L. Rocher",
            "J. M. Hendrickx",
            "Y.-A. de Montjoye",
            "Arvind Narayanan",
            "Vitaly Shmatikov",
        ] {
            assert!(names.iter().any(|x| x == n), "{n} missing from {names:?}");
        }
        assert!(
            !names.iter().any(|x| x.contains("Tipene")),
            "prose before the references: {names:?}"
        );
    }

    #[test]
    fn a_cited_surname_alone_covers_only_the_surname() {
        let public = vec!["Sweeney".to_string()];
        assert!(is_public("Sweeney", &public));
        assert!(!is_public("Aroha Sweeney", &public));
        let public = vec!["Sweeney, L.".to_string()];
        assert!(is_public("Latanya Sweeney", &public));
        assert!(is_public("Sweeney", &public));
    }

    #[test]
    fn the_byline_ends_at_the_abstract() {
        let paper = "k-Anonymity\nLatanya Sweeney\nSchool of Computer Science\n\nAbstract\nConsider a data holder…";
        let end = byline_end(paper).unwrap();
        assert!(paper[..end].contains("Latanya"));
        assert!(byline_end("Kia ora,\nHe pātai tāku.").is_none());
    }

    #[test]
    fn public_names_match_their_other_forms() {
        let public = vec!["Latanya Sweeney".to_string()];
        assert!(is_public("Sweeney, L.", &public));
        assert!(is_public("LATANYA SWEENEY", &public));
        assert!(!is_public("Aroha Sweeney", &public));
        assert!(!is_public("Latanya", &public), "a first name alone is not enough");
    }
}
