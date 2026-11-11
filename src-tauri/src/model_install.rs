//! Installs the Tūmataiti name model when the person asks for it.
//!
//! Downloads come from two places only: the pinned revision of the model on
//! Hugging Face, and Microsoft's ONNX Runtime release on GitHub. Redirects are
//! followed only over HTTPS to those services' own download hosts. Every file
//! is checked against a SHA-256 compiled into Pātaka before it is kept, so a
//! changed or substituted file is refused. Nothing personal is sent.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::student::{self, Runtime, MODEL_FILES, MODEL_REPO, MODEL_REVISION, RUNTIME};

/// Hosts a download may be redirected to.
const ALLOWED_HOST_SUFFIXES: [&str; 4] = ["huggingface.co", ".hf.co", "github.com", ".githubusercontent.com"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub stage: String,
    pub done: u64,
    pub total: u64,
}

fn host_allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some_and(|h| {
            ALLOWED_HOST_SUFFIXES
                .iter()
                .any(|s| if s.starts_with('.') { h.ends_with(s) } else { h == *s })
        })
}

pub fn download_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("Pataka/", env!("CARGO_PKG_VERSION")))
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || !host_allowed(attempt.url()) {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(120))
        .build()
        .expect("download client builds")
}

/// Stream `url` to `dest`, refusing anything over `bytes` or whose SHA-256
/// differs from `sha256`. Written to a temporary file and renamed only once verified.
async fn fetch(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    sha256: &str,
    bytes: u64,
    stage: &str,
    progress: &(dyn Fn(Progress) + Send + Sync),
) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| Error::Invalid("bad download URL".into()))?;
    if !host_allowed(&parsed) {
        return Err(Error::Invalid("download host not allowed".into()));
    }
    let resp = client.get(parsed).send().await?;
    if !resp.status().is_success() {
        return Err(Error::Network(format!(
            "{stage}: the server answered {}",
            resp.status().as_u16()
        )));
    }
    let tmp = dest.with_extension("partial");
    let result = async {
        let mut file = fs::File::create(&tmp)?;
        let mut hasher = Sha256::new();
        let mut done = 0u64;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            done += chunk.len() as u64;
            if done > bytes {
                return Err(Error::Invalid(format!("{stage}: larger than the pinned file")));
            }
            hasher.update(&chunk);
            file.write_all(&chunk)?;
            progress(Progress {
                stage: stage.to_string(),
                done,
                total: bytes,
            });
        }
        file.sync_all()?;
        if hex::encode(hasher.finalize()) != sha256 {
            return Err(Error::Invalid(format!(
                "{stage}: the download does not match its pinned SHA-256, so it was discarded"
            )));
        }
        fs::rename(&tmp, dest)?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Pull the one library file out of the ONNX Runtime archive.
fn extract(archive: &Path, rt: &Runtime, dest: &Path) -> Result<()> {
    let want = rt.member.trim_start_matches("./");
    let tmp = dest.with_extension("partial");
    let mut out = fs::File::create(&tmp)?;
    let found = if rt.url.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(fs::File::open(archive)?).map_err(|_| Error::Integrity)?;
        let mut entry = zip
            .by_name(want)
            .map_err(|_| Error::Invalid("ONNX Runtime archive is missing its library".into()))?;
        std::io::copy(&mut entry, &mut out)?;
        true
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(archive)?));
        let mut found = false;
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.to_string_lossy().trim_start_matches("./").to_string();
            if path == want {
                std::io::copy(&mut entry, &mut out)?;
                found = true;
                break;
            }
        }
        found
    };
    out.sync_all()?;
    drop(out);
    if !found {
        let _ = fs::remove_file(&tmp);
        return Err(Error::Invalid("ONNX Runtime archive is missing its library".into()));
    }
    if student::sha256_file(&tmp)? != rt.library_sha256 {
        let _ = fs::remove_file(&tmp);
        return Err(Error::Invalid(
            "The ONNX Runtime library does not match its pinned SHA-256.".into(),
        ));
    }
    fs::rename(&tmp, dest)?;
    Ok(())
}

pub fn download_size() -> u64 {
    RUNTIME.map(|r| r.archive_bytes).unwrap_or(0) + MODEL_FILES.iter().map(|f| f.bytes).sum::<u64>()
}

pub async fn install(client: &reqwest::Client, root: &Path, progress: &(dyn Fn(Progress) + Send + Sync)) -> Result<()> {
    let rt = RUNTIME.ok_or_else(|| Error::Invalid("ONNX Runtime has no build for this platform, so the name model cannot run here. The rules and kaitiaki lists still work.".into()))?;
    let dir = student::model_dir(root);
    fs::create_dir_all(&dir)?;
    let lib = student::runtime_path(root).expect("runtime is Some");
    let lib_dir: PathBuf = lib.parent().expect("has parent").to_path_buf();
    fs::create_dir_all(&lib_dir)?;

    if !lib.is_file() || student::sha256_file(&lib)? != rt.library_sha256 {
        let archive = lib_dir.join("download.archive");
        fetch(
            client,
            rt.url,
            &archive,
            rt.archive_sha256,
            rt.archive_bytes,
            "ONNX Runtime",
            progress,
        )
        .await?;
        let (a, l) = (archive.clone(), lib.clone());
        let r = tauri::async_runtime::spawn_blocking(move || extract(&a, &rt, &l))
            .await
            .map_err(|_| Error::Invalid("extracting ONNX Runtime failed".into()))?;
        let _ = fs::remove_file(&archive);
        r?;
    }
    for f in &MODEL_FILES {
        let dest = dir.join(f.local);
        if dest.is_file() && student::sha256_file(&dest)? == f.sha256 {
            continue;
        }
        let url = format!(
            "https://huggingface.co/{MODEL_REPO}/resolve/{MODEL_REVISION}/{}",
            f.remote
        );
        fetch(client, &url, &dest, f.sha256, f.bytes, f.local, progress).await?;
    }
    Ok(())
}

pub fn remove(root: &Path) -> Result<()> {
    let models = root.join("models");
    if models.exists() {
        fs::remove_dir_all(models)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_to_known_hosts() {
        let ok = |s: &str| host_allowed(&reqwest::Url::parse(s).unwrap());
        assert!(ok("https://huggingface.co/x"));
        assert!(ok("https://us.aws.cdn.hf.co/x"));
        assert!(ok("https://github.com/microsoft/onnxruntime/releases/download/x"));
        assert!(ok("https://release-assets.githubusercontent.com/x"));
        assert!(!ok("http://huggingface.co/x"));
        assert!(!ok("https://evilhf.co/x"));
        assert!(!ok("https://huggingface.co.evil.com/x"));
        assert!(!ok("https://example.com/x"));
    }
}
