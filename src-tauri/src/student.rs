//! The Tūmataiti student model, run natively.
//!
//! A DeBERTa-v3-small token classifier fine-tuned by Tūmataiti to find names,
//! addresses, affiliation and the other open-ended categories the rules
//! cannot (SPEC §6.1). It runs on this device with ONNX Runtime; nothing is
//! sent anywhere.
//!
//! Tokenisation into overlapping windows and decoding of per-token tags back
//! into spans are ported from `crates/tumataiti-student` (Apache-2.0) so that
//! Pātaka produces the same spans as the Tūmataiti demo and evaluation. Its
//! spans then go through Tūmataiti's own review and merge steps with the rules.
//!
//! Every file is pinned by SHA-256 and checked each time the model loads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use ort::session::Session;
use ort::value::Tensor;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokenizers::{Tokenizer, TruncationDirection, TruncationParams, TruncationStrategy};
use tumataiti_rules::{Category, Checksum, Source, Span};

use crate::error::{Error, Result};

// ------------------------------------------------------------------ pins

/// The published student, as pinned in Tūmataiti's `demo/model.lock`.
pub const MODEL_REPO: &str = "woodrock/tumataiti-deberta-v3-small-open";
pub const MODEL_REVISION: &str = "b48e3be22f5ce05643d22cba5f22b50311fe57f7";
pub const MODEL_NAME: &str = "deberta-v3-small-open";
pub const MODEL_PRECISION: &str = "int8 embeddings, int4 encoder";

pub struct Pinned {
    /// Path in the Hugging Face repo.
    pub remote: &'static str,
    /// File name on disk.
    pub local: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

pub const MODEL_FILES: [Pinned; 3] = [
    Pinned {
        remote: "onnx/model_int4.onnx",
        local: "model_int4.onnx",
        sha256: "9eb92686930fbf2ad8e86b4421393319b6e5675778f6cb316da538d13bd0b27a",
        bytes: 151_944_048,
    },
    Pinned {
        remote: "tokenizer.json",
        local: "tokenizer.json",
        sha256: "8196ef6e837c3ed355ae78bf29a244a33f4dc4179a7c364aedeb0cd6433e9948",
        bytes: 8_339_976,
    },
    Pinned {
        remote: "config.json",
        local: "config.json",
        sha256: "453eca0aef93d0a3c848453b72de06117a2361319d2e83f61df39d9f2d75517b",
        bytes: 1_805,
    },
];

/// ONNX Runtime, from Microsoft's GitHub release, for this platform.
pub struct Runtime {
    pub url: &'static str,
    pub archive_sha256: &'static str,
    pub archive_bytes: u64,
    /// Path of the library inside the archive.
    pub member: &'static str,
    pub library: &'static str,
    pub library_sha256: &'static str,
}

pub const ORT_VERSION: &str = "1.31.0";

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const RUNTIME: Option<Runtime> = Some(Runtime {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.31.0/onnxruntime-linux-x64-1.31.0.tgz",
    archive_sha256: "cc5c72baf5ae5c8238a6841f0897227be2d02826b9cf98eaf02fdefaeeb03a57",
    archive_bytes: 11_408_003,
    member: "onnxruntime-linux-x64-1.31.0/lib/libonnxruntime.so.1.31.0",
    library: "libonnxruntime.so.1.31.0",
    library_sha256: "469849977cb60744ea160927e1a3a0643fd4c55c0aff743251c6d293402e334e",
});

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub const RUNTIME: Option<Runtime> = Some(Runtime {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.31.0/onnxruntime-linux-aarch64-1.31.0.tgz",
    archive_sha256: "c5b8b3cca31f3d643a3b313b8f42e2d0f16a4fcefd02cda343d97bd5306afaab",
    archive_bytes: 10_331_104,
    member: "onnxruntime-linux-aarch64-1.31.0/lib/libonnxruntime.so.1.31.0",
    library: "libonnxruntime.so.1.31.0",
    library_sha256: "595c7f7a6775f5c7b3d567c3ed94c04f722092575abcc092560f5208a54daf99",
});

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const RUNTIME: Option<Runtime> = Some(Runtime {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.31.0/onnxruntime-osx-arm64-1.31.0.tgz",
    archive_sha256: "1031b36dd3bdaa8d976a6130d9cf2a05e9be320b93ae804f8427b91dc3432afc",
    archive_bytes: 36_549_009,
    member: "./onnxruntime-osx-arm64-1.31.0/lib/libonnxruntime.1.31.0.dylib",
    library: "libonnxruntime.1.31.0.dylib",
    library_sha256: "2a50a63e7f41f8d4d551d3ee47248df3864f6540109fce8ad520ca3b51af4b70",
});

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub const RUNTIME: Option<Runtime> = Some(Runtime {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.31.0/onnxruntime-win-x64-1.31.0.zip",
    archive_sha256: "3958d8a44984160692c1b4ffd0de3cb48f8c39f9d34bb31e433db06e30b82195",
    archive_bytes: 82_887_349,
    member: "onnxruntime-win-x64-1.31.0/lib/onnxruntime.dll",
    library: "onnxruntime.dll",
    library_sha256: "0c42dc4e278f0968fdf9ebf1b1e54a74c657a9942e70803e453c91993451cf4c",
});

/// ONNX Runtime publishes no build for this platform (e.g. Intel Macs): the
/// rules and kaitiaki lists still work, the model does not.
#[cfg(not(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "windows", target_arch = "x86_64"),
)))]
pub const RUNTIME: Option<Runtime> = None;

/// Where the model lives under the app data directory.
pub fn model_dir(root: &Path) -> PathBuf {
    root.join("models")
        .join(format!("{MODEL_NAME}@{}", &MODEL_REVISION[..7]))
}

pub fn runtime_path(root: &Path) -> Option<PathBuf> {
    RUNTIME.map(|r| {
        root.join("models")
            .join(format!("onnxruntime-{ORT_VERSION}"))
            .join(r.library)
    })
}

pub fn detector_name() -> String {
    format!("{MODEL_NAME}@{}", &MODEL_REVISION[..7])
}

pub fn is_installed(root: &Path) -> bool {
    let dir = model_dir(root);
    RUNTIME.is_some()
        && MODEL_FILES.iter().all(|f| dir.join(f.local).is_file())
        && runtime_path(root).is_some_and(|p| p.is_file())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

fn verify(path: &Path, expected: &str) -> Result<()> {
    if sha256_file(path)? == expected {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "{} does not match its pinned SHA-256. Remove the name model in Settings and install it again.",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("A model file")
        )))
    }
}

// ------------------------------------------------------------------ model

/// Identifier field labels, never PII themselves (`FIELD_LABELS` upstream).
const FIELD_LABELS: [&str; 21] = [
    "NHI", "IRD", "GST", "DOB", "DL", "SSN", "PIN", "CVV", "CVC", "IBAN", "BSB", "TFN", "NZBN", "ACC", "NI", "ID",
    "SIN", "EIN", "ITIN", "MRN", "VIN",
];
/// Window length and overlap in tokens, as in training.
const MAX_LENGTH: usize = 512;
const STRIDE: usize = 128;
/// Decoding threshold on P(not O), as in training and the demo.
const THRESHOLD: f32 = 0.5;
const CACHE_LIMIT: usize = 4096;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelSpan {
    /// UTF-16 offsets (as upstream), converted to bytes in [`Student::detect`].
    pub start: usize,
    pub end: usize,
    pub label: String,
    pub confidence: f32,
}

static ENVIRONMENT: OnceLock<std::result::Result<ort::environment::Environment, String>> = OnceLock::new();

fn environment(library: &Path) -> Result<&'static ort::environment::Environment> {
    ENVIRONMENT
        .get_or_init(|| {
            ort::init_from(library)
                .map_err(|e| e.to_string())?
                .with_name("pataka")
                // ONNX Runtime can send telemetry (Windows ETW). Pātaka sends none.
                .with_telemetry(false)
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| Error::Invalid(format!("ONNX Runtime could not be loaded: {e}")))
}

pub struct Student {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    tags: Vec<String>,
    cache: Mutex<HashMap<[u8; 32], Vec<ModelSpan>>>,
}

impl Student {
    /// Check every pinned file, then load ONNX Runtime, the tokeniser and the model.
    pub fn load(root: &Path) -> Result<Student> {
        let rt = RUNTIME.ok_or_else(|| Error::Invalid("The name model is not available on this platform.".into()))?;
        let lib = runtime_path(root).expect("runtime is Some");
        let dir = model_dir(root);
        if !is_installed(root) {
            return Err(Error::Invalid("The name model is not installed.".into()));
        }
        verify(&lib, rt.library_sha256)?;
        for f in &MODEL_FILES {
            verify(&dir.join(f.local), f.sha256)?;
        }
        let env = environment(&lib)?;
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().min(4))
            .unwrap_or(2);
        let session = Session::builder(env)
            .and_then(|b| b.with_intra_threads(threads).map_err(Into::into))
            .and_then(|mut b| b.commit_from_file(dir.join(MODEL_FILES[0].local)))
            .map_err(|e| Error::Invalid(format!("The name model could not be loaded: {e}")))?;
        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| Error::Invalid(format!("The model tokeniser could not be loaded: {e}")))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_LENGTH,
                stride: STRIDE,
                strategy: TruncationStrategy::LongestFirst,
                direction: TruncationDirection::Right,
            }))
            .map_err(|e| Error::Invalid(e.to_string()))?;
        tokenizer.with_padding(None);
        let config = std::fs::read_to_string(dir.join("config.json"))?;
        Ok(Student {
            session: Mutex::new(session),
            tokenizer,
            tags: tags_from_config(&config)?,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Spans for `text` (UTF-16 offsets), cached by the text's hash.
    fn model_spans(&self, text: &str) -> Result<Vec<ModelSpan>> {
        let key: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        if let Some(hit) = self.cache.lock().unwrap_or_else(|p| p.into_inner()).get(&key) {
            return Ok(hit.clone());
        }
        let enc = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| Error::Invalid(format!("tokeniser: {e}")))?;
        let utf16 = utf16_offsets(text);
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut found = Vec::new();
        let mut session = self.session.lock().unwrap_or_else(|p| p.into_inner());
        for e in std::iter::once(&enc).chain(enc.get_overflowing()) {
            let ids: Vec<i64> = e.get_ids().iter().map(|&i| i as i64).collect();
            let offsets: Vec<(usize, usize)> = e
                .get_offsets()
                .iter()
                .zip(e.get_special_tokens_mask())
                .map(|(&(a, b), &special)| if special == 1 { (0, 0) } else { (utf16[a], utf16[b]) })
                .collect();
            let n = ids.len();
            let input_ids = Tensor::from_array(([1usize, n], ids)).map_err(|e| Error::Invalid(e.to_string()))?;
            let mask = Tensor::from_array(([1usize, n], vec![1i64; n])).map_err(|e| Error::Invalid(e.to_string()))?;
            let outputs = session
                .run(ort::inputs!["input_ids" => input_ids, "attention_mask" => mask])
                .map_err(|e| Error::Invalid(format!("The name model failed: {e}")))?;
            let (_, logits) = outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| Error::Invalid(format!("model output: {e}")))?;
            if logits.len() != n * self.tags.len() {
                return Err(Error::Invalid("The name model returned an unexpected shape.".into()));
            }
            found.extend(decode(&units, &offsets, logits, &self.tags, THRESHOLD));
        }
        let spans = union(found);
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, spans.clone());
        Ok(spans)
    }

    /// Model spans as Tūmataiti [`Span`]s with byte offsets, ready to merge
    /// with the rules (as `tumataiti-wasm`'s `redactWith` does).
    pub fn detect(&self, text: &str) -> Result<Vec<Span>> {
        let bytes = byte_offsets(text);
        let mut out = Vec::new();
        for m in self.model_spans(text)? {
            let Some(category) = model_category(&m.label) else {
                continue;
            };
            let start = bytes[m.start.min(bytes.len() - 1)];
            let end = bytes[m.end.min(bytes.len() - 1)];
            if start >= end {
                continue;
            }
            out.push(Span {
                start,
                end,
                text: text[start..end].to_string(),
                category,
                confidence: m.confidence,
                source: Source::Model,
                detector: detector_name(),
                checksum: Checksum::None,
                note: None,
            });
        }
        Ok(out)
    }
}

/// Student label → category (`model_category` in `tumataiti-wasm`). `ID` is
/// relabelled by Tūmataiti's review step from its check digits.
fn model_category(label: &str) -> Option<Category> {
    Some(match label {
        "PERSON" => Category::Person,
        "ADDRESS" => Category::Address,
        "EMAIL" => Category::Email,
        "PHONE" => Category::Phone,
        "URL" => Category::Url,
        "IP_ADDRESS" => Category::IpAddress,
        "DATE" => Category::Date,
        "ID" => Category::NzGovId,
        "SECRET" => Category::Secret,
        "AFFILIATION" => Category::Affiliation,
        _ => return None,
    })
}

fn tags_from_config(config_json: &str) -> Result<Vec<String>> {
    let config: serde_json::Value = serde_json::from_str(config_json)?;
    let id2label = config["id2label"]
        .as_object()
        .ok_or_else(|| Error::Invalid("config.json has no id2label".into()))?;
    let mut tags = vec![String::new(); id2label.len()];
    for (id, label) in id2label {
        let i: usize = id.parse().map_err(|_| Error::Invalid("bad id2label".into()))?;
        *tags
            .get_mut(i)
            .ok_or_else(|| Error::Invalid("id2label ids are not 0..n".into()))? =
            label.as_str().unwrap_or_default().to_string();
    }
    if tags.first().map(String::as_str) != Some("O") {
        return Err(Error::Invalid("tag 0 must be O".into()));
    }
    Ok(tags)
}

/// Byte offset → UTF-16 offset, for every char boundary in `text`.
fn utf16_offsets(text: &str) -> Vec<usize> {
    let mut map = vec![0; text.len() + 1];
    let mut pos = 0;
    for (byte, c) in text.char_indices() {
        map[byte] = pos;
        pos += c.len_utf16();
    }
    map[text.len()] = pos;
    map
}

/// UTF-16 offset → byte offset; offsets inside a surrogate pair round down.
fn byte_offsets(text: &str) -> Vec<usize> {
    let mut map = Vec::with_capacity(text.len() + 1);
    for (byte, c) in text.char_indices() {
        map.extend(std::iter::repeat_n(byte, c.len_utf16()));
    }
    map.push(text.len());
    map
}

fn is_space(unit: u16) -> bool {
    char::from_u32(unit as u32).is_some_and(char::is_whitespace)
}

fn is_word(unit: u16) -> bool {
    char::from_u32(unit as u32).is_some_and(char::is_alphanumeric)
}

/// Spans from one window's logits; see `decode` in `tumataiti-student`.
fn decode(text: &[u16], offsets: &[(usize, usize)], logits: &[f32], tags: &[String], threshold: f32) -> Vec<ModelSpan> {
    let n = tags.len();
    let mut spans: Vec<ModelSpan> = Vec::new();
    let mut prev: Option<&str> = None;
    for (t, &(a, b)) in offsets.iter().enumerate() {
        if a == b {
            continue;
        }
        let row = &logits[t * n..(t + 1) * n];
        let max = row.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let exp: Vec<f32> = row.iter().map(|x| (x - max).exp()).collect();
        let sum: f32 = exp.iter().sum();
        let conf = 1.0 - exp[0] / sum;
        if conf < threshold {
            prev = None;
            continue;
        }
        let best = (1..n)
            .max_by(|&i, &j| exp[i].total_cmp(&exp[j]))
            .expect("at least one entity tag");
        let (kind, label) = tags[best].split_once('-').unwrap_or(("B", &tags[best]));
        match spans.last_mut() {
            Some(last) if kind == "I" && prev == Some(label) => {
                last.end = b;
                last.confidence = last.confidence.min(conf);
            }
            _ => spans.push(ModelSpan {
                start: a,
                end: b,
                label: label.to_string(),
                confidence: conf,
            }),
        }
        prev = Some(label);
    }
    spans
        .into_iter()
        .filter_map(|mut s| {
            while s.start < s.end && is_space(text[s.start]) {
                s.start += 1;
            }
            while s.end > s.start && is_space(text[s.end - 1]) {
                s.end -= 1;
            }
            if s.start == s.end {
                return None;
            }
            while s.start > 0 && is_word(text[s.start - 1]) && is_word(text[s.start]) {
                s.start -= 1;
            }
            while s.end < text.len() && is_word(text[s.end]) && is_word(text[s.end - 1]) {
                s.end += 1;
            }
            let word = String::from_utf16_lossy(&text[s.start..s.end]).to_uppercase();
            (!FIELD_LABELS.contains(&word.as_str())).then_some(s)
        })
        .collect()
}

/// Merge overlapping spans from different windows; see `union` upstream.
fn union(mut spans: Vec<ModelSpan>) -> Vec<ModelSpan> {
    spans.sort_by(|x, y| {
        (x.start, x.end, &x.label)
            .cmp(&(y.start, y.end, &y.label))
            .then(x.confidence.total_cmp(&y.confidence))
    });
    let mut merged: Vec<ModelSpan> = Vec::new();
    for s in spans {
        match merged.last_mut() {
            Some(last) if s.start < last.end => {
                if s.end - s.start > last.end - last.start {
                    last.label = s.label;
                }
                last.end = last.end.max(s.end);
                last.confidence = last.confidence.max(s.confidence);
            }
            _ => merged.push(s),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags() -> Vec<String> {
        ["O", "B-PERSON", "I-PERSON", "B-EMAIL", "I-EMAIL"]
            .map(String::from)
            .to_vec()
    }

    fn logits(tag_per_token: &[usize]) -> Vec<f32> {
        tag_per_token
            .iter()
            .flat_map(|&t| (0..5).map(move |i| if i == t { 10.0 } else { 0.0 }))
            .collect()
    }

    // The upstream tests, so the port stays in step with tumataiti-student.
    #[test]
    fn decode_joins_inside_tags_and_trims_space() {
        let text: Vec<u16> = "Kia ora Hēmi Ngata!".encode_utf16().collect();
        let offsets = [(0, 0), (0, 3), (3, 7), (7, 12), (12, 18), (18, 19)];
        let spans = decode(&text, &offsets, &logits(&[1, 0, 0, 1, 2, 0]), &tags(), 0.5);
        assert_eq!(spans.len(), 1);
        assert_eq!(
            (spans[0].start, spans[0].end, spans[0].label.as_str()),
            (8, 18, "PERSON")
        );
    }

    #[test]
    fn spans_widen_to_whole_words() {
        let text: Vec<u16> = "NHI ZZZ0016, Ngata's".encode_utf16().collect();
        let spans = decode(
            &text,
            &[(2, 3), (3, 4), (13, 16), (16, 18)],
            &logits(&[3, 0, 1, 0]),
            &tags(),
            0.5,
        );
        let got: Vec<(usize, usize)> = spans.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(got, [(13, 18)]);
    }

    #[test]
    fn inside_tag_after_other_label_starts_new_span() {
        let text: Vec<u16> = "a b".encode_utf16().collect();
        let spans = decode(&text, &[(0, 1), (1, 3)], &logits(&[3, 2]), &tags(), 0.5);
        assert_eq!(
            spans.iter().map(|s| s.label.as_str()).collect::<Vec<_>>(),
            ["EMAIL", "PERSON"]
        );
    }

    #[test]
    fn union_keeps_the_longer_label() {
        let s = |start, end, label: &str| ModelSpan {
            start,
            end,
            label: label.into(),
            confidence: 0.9,
        };
        let merged = union(vec![s(5, 9, "PERSON"), s(0, 4, "EMAIL"), s(3, 12, "ADDRESS")]);
        assert_eq!(
            merged,
            vec![ModelSpan {
                start: 0,
                end: 12,
                label: "ADDRESS".into(),
                confidence: 0.9
            }]
        );
    }

    #[test]
    fn offsets_round_trip_with_macrons() {
        let t = "Hēmi 😀 Ngata";
        let u = utf16_offsets(t);
        let b = byte_offsets(t);
        for (byte, _) in t.char_indices() {
            assert_eq!(b[u[byte]], byte);
        }
    }

    /// Runs the real model when it is installed: set PATAKA_MODEL_ROOT to a
    /// directory laid out like the app data directory.
    #[test]
    #[ignore]
    fn real_model_finds_names() {
        let root = PathBuf::from(std::env::var("PATAKA_MODEL_ROOT").expect("PATAKA_MODEL_ROOT"));
        let s = Student::load(&root).unwrap();
        let text = "Kia ora, I spoke with Hēmi Te Rangi from Ngāti Porou at 12 Kōwhai Street, Ōtaki yesterday.";
        let spans = s.detect(text).unwrap();
        for sp in &spans {
            eprintln!("{:?} {:?} {:.2}", sp.category, sp.text, sp.confidence);
        }
        assert!(spans
            .iter()
            .any(|s| s.category == Category::Person && s.text.contains("Hēmi")));
    }
}
