//! Bounded media reference loading, with canonical local-root checks.

use std::io::Read;
use std::path::Path;

use main_core::packages::ai_provider::media_protocols::{MediaBlob, base64_to_bytes};
use main_core::packages::electron_utils::generated_images::read_generated_image;
use shell_contract::runtime::CancelToken;

const LIMIT: usize = 200 * 1024 * 1024;
const MAX_ITEMS: usize = 12;

fn mime(name: &str) -> Option<&'static str> {
    match Path::new(name)
        .extension()?
        .to_str()?
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "heic" => Some("image/heic"),
        "mp4" | "m4v" => Some("video/mp4"),
        "mov" => Some("video/quicktime"),
        "webm" => Some("video/webm"),
        "mkv" => Some("video/x-matroska"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "m4a" => Some("audio/mp4"),
        "aac" => Some("audio/aac"),
        "ogg" => Some("audio/ogg"),
        "flac" => Some("audio/flac"),
        _ => None,
    }
}

fn data_parts(reference: &str) -> Option<(&str, &str)> {
    let (header, data) = reference.strip_prefix("data:")?.split_once(",")?;
    let kind = header.strip_suffix(";base64")?;
    if kind.is_empty() || kind.contains(';') || kind.contains(',') {
        return None;
    }
    Some((kind, data))
}

fn local(reference: &str, roots: &[String]) -> Result<MediaBlob, String> {
    let kind = mime(reference).ok_or_else(|| {
        format!("Unsupported media file: {reference} (images, video and audio only)")
    })?;
    if !Path::new(reference).exists() {
        return Err(format!("File not found: {reference}"));
    }
    let path = std::fs::canonicalize(reference).map_err(|error| error.to_string())?;
    if !roots.is_empty()
        && !roots
            .iter()
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .any(|root| path.starts_with(root))
    {
        return Err(format!(
            "Refusing to read a local media file outside the allowed directories: {reference} (allowed: {})",
            roots.join(", ")
        ));
    }
    let file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
    let meta = file.metadata().map_err(|error| error.to_string())?;
    if !meta.is_file() {
        return Err(format!("Not a regular file: {reference}"));
    }
    if meta.len() > LIMIT as u64 {
        return Err(format!("{reference} is too large to analyze"));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > LIMIT {
        return Err(format!("{reference} is too large to analyze"));
    }
    Ok(MediaBlob {
        bytes,
        mime: kind.into(),
        name: Path::new(reference)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned()),
    })
}

/// `download` must validate every redirect and pin the validated address.
pub(super) fn load(
    refs: &[String],
    roots: &[String],
    cancel: &CancelToken,
    download: &impl Fn(&str, &CancelToken) -> Result<(Vec<u8>, Option<String>, Option<String>), String>,
) -> Result<Vec<MediaBlob>, String> {
    if refs.len() > MAX_ITEMS {
        return Err(format!(
            "Too many media items in one request ({}, limit {MAX_ITEMS}); analyze them in smaller batches",
            refs.len()
        ));
    }
    let mut declared = 0usize;
    for reference in refs {
        if let Some((_, body)) = data_parts(reference) {
            let size = body.len().saturating_mul(3) / 4;
            if size > LIMIT {
                return Err(format!(
                    "data URL is too large to analyze (limit {LIMIT} bytes)"
                ));
            }
            declared = declared.saturating_add(size);
        }
    }
    if declared > LIMIT {
        return Err(format!(
            "Media in one request is too large to analyze ({declared} bytes, limit {LIMIT}); analyze it in smaller batches"
        ));
    }
    let mut total = 0usize;
    let mut result = Vec::with_capacity(refs.len());
    for reference in refs {
        if cancel.is_cancelled() {
            return Err("This operation was aborted".into());
        }
        let blob = if reference.starts_with("data:") {
            let (kind, body) = data_parts(reference).ok_or_else(|| {
                "Unsupported data URL: only base64-encoded media can be analyzed".to_string()
            })?;
            let bytes = base64_to_bytes(body);
            if bytes.len() > LIMIT {
                return Err("data URL is too large to analyze".into());
            }
            MediaBlob {
                bytes,
                mime: kind.to_lowercase(),
                name: None,
            }
        } else if reference.starts_with("file:") {
            let image = read_generated_image(&std::env::temp_dir().to_string_lossy(), reference)
                .ok_or_else(|| format!("Not an accessible image: {reference}"))?;
            MediaBlob {
                bytes: image.bytes,
                mime: image.mime,
                name: None,
            }
        } else if reference.to_ascii_lowercase().starts_with("https://")
            || reference.to_ascii_lowercase().starts_with("http://")
        {
            let (bytes, content_type, name) = download(reference, cancel)?;
            let kind = content_type
                .as_deref()
                .map(|s| {
                    s.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                })
                .filter(|s| !s.is_empty() && s != "application/octet-stream")
                .or_else(|| name.as_deref().and_then(mime).map(str::to_owned))
                .ok_or_else(|| format!("Could not tell the media type of {reference}"))?;
            MediaBlob {
                bytes,
                mime: kind,
                name,
            }
        } else {
            local(reference, roots)?
        };
        total = total.saturating_add(blob.bytes.len());
        if total > LIMIT {
            return Err(format!(
                "Media in one request is too large to analyze (over {LIMIT} bytes); analyze it in smaller batches"
            ));
        }
        result.push(blob);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn no_network(
        _: &str,
        _: &CancelToken,
    ) -> Result<(Vec<u8>, Option<String>, Option<String>), String> {
        Err("network not expected".into())
    }

    #[test]
    fn loads_inline_media_and_respects_cancellation() {
        let cancel = CancelToken::default();
        let loaded = load(
            &["data:image/png;base64,YWJj".into()],
            &[],
            &cancel,
            &no_network,
        )
        .unwrap();
        assert_eq!(loaded[0].bytes, b"abc");
        assert_eq!(loaded[0].mime, "image/png");
        cancel.cancel();
        assert!(
            load(
                &["data:image/png;base64,YWJj".into()],
                &[],
                &cancel,
                &no_network
            )
            .is_err()
        );
        assert!(
            load(
                &vec!["x".into(); 13],
                &[],
                &CancelToken::default(),
                &no_network
            )
            .is_err()
        );
    }

    #[test]
    fn local_media_cannot_escape_its_root() {
        let root = std::env::temp_dir().join(format!(
            "media-reference-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let allowed = root.join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let inside = allowed.join("inside.png");
        let outside = root.join("outside.png");
        std::fs::write(&inside, b"inside").unwrap();
        std::fs::write(&outside, b"outside").unwrap();
        let roots = vec![allowed.to_string_lossy().into_owned()];
        let token = CancelToken::default();
        let result = load(
            &[inside.to_string_lossy().into_owned()],
            &roots,
            &token,
            &no_network,
        )
        .unwrap();
        assert_eq!(result[0].bytes, b"inside");
        assert!(
            load(
                &[outside.to_string_lossy().into_owned()],
                &roots,
                &token,
                &no_network
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
