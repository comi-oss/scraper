//! Capture public Instagram carousels through the visible browser UI.
use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Deserialize)]
pub(crate) struct Slide {
    pub(crate) index: usize,
    pub(crate) file: PathBuf,
}
#[derive(Debug, Deserialize)]
pub(crate) struct Capture {
    pub(crate) source_url: String,
    pub(crate) status: String,
    pub(crate) reason: Option<String>,
    pub(crate) captured_at: String,
    pub(crate) slides: Vec<Slide>,
    #[serde(skip)]
    pub(crate) manifest: PathBuf,
}

pub(crate) fn shortcode(source: &str) -> Option<String> {
    let url = Url::parse(source).ok()?;
    if url.scheme() != "https" || !matches!(url.host_str()?, "instagram.com" | "www.instagram.com")
    {
        return None;
    }
    let parts: Vec<_> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let pos = parts.iter().position(|s| *s == "p")?;
    let code = *parts.get(pos + 1)?;
    (!code.is_empty()
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')))
    .then(|| code.to_owned())
}

pub(crate) fn capture(source: &str, max_slides: usize) -> Result<Capture> {
    let code = shortcode(source).context("unsupported Instagram post URL")?;
    let output = Path::new("data/instagram-captures").join(&code);
    std::fs::create_dir_all(&output)?;
    let manifest = output.join("manifest.json");
    // Completed manifests are reusable for a day. Partial results must be retried.
    if let Ok(raw) = std::fs::read(&manifest)
        && let Ok(mut saved) = serde_json::from_slice::<Capture>(&raw)
        && saved.status == "complete"
        && saved.slides.len() <= max_slides
        && saved.slides.iter().all(|s| s.file.is_file())
        && chrono::DateTime::parse_from_rfc3339(&saved.captured_at)
            .is_ok_and(|time| chrono::Utc::now().signed_duration_since(time).num_hours() < 24)
    {
        saved.manifest = manifest;
        return Ok(saved);
    }
    let script = std::env::var_os("COMI_INSTAGRAM_CAPTURE_SCRIPT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/instagram-screenshotter.mjs")
        });
    let result = Command::new("node")
        .arg(script)
        .arg(source)
        .arg("--output")
        .arg(&output)
        .arg("--max-slides")
        .arg(max_slides.min(50).to_string())
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("CLOUDFLARE_API_TOKEN")
        .output()
        .context("could not launch Instagram screenshotter; install Node.js and run npm ci")?;
    std::fs::write(output.join("capture.log"), &result.stderr)?;
    ensure!(
        result.status.success() || result.status.code() == Some(2),
        "Instagram browser capture failed; see capture.log"
    );
    let mut capture: Capture =
        serde_json::from_slice(&result.stdout).context("invalid screenshotter manifest")?;
    ensure!(
        shortcode(&capture.source_url).as_deref() == Some(&code),
        "screenshotter returned a different post"
    );
    let output = output.canonicalize()?;
    for slide in &capture.slides {
        ensure!(
            slide.file.canonicalize()?.starts_with(&output),
            "screenshot path is outside capture directory"
        );
    }
    capture.manifest = manifest;
    Ok(capture)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_only_public_instagram_post_routes() {
        assert_eq!(
            shortcode("https://www.instagram.com/artist/p/ABC_123/?img_index=2").as_deref(),
            Some("ABC_123")
        );
        assert!(shortcode("https://evil.example/p/ABC/").is_none());
        assert!(shortcode("https://www.instagram.com/accounts/login/").is_none());
    }
}
