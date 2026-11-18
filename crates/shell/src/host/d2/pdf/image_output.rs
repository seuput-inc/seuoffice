//! Write structure-extracted PNGs without replacing existing files.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use jscompat::node_path::host;
use shell_contract::{IpcValue, runtime::CancelToken};
use shell_ipc_d2::pdf::tools::JobFailure;

pub(super) struct ExtractedImage {
    pub page: f64,
    pub width: f64,
    pub height: f64,
    pub png: Vec<u8>,
}

pub(super) fn write_images(
    out_dir: &str,
    base_name: &str,
    images: &[ExtractedImage],
    progress: &(dyn Fn(f64, f64) + Sync),
    cancel: &CancelToken,
) -> Result<IpcValue, JobFailure> {
    let sanitized: String = base_name.chars().map(|c| if c <= '\u{1f}' || "\\/:*?\"<>|".contains(c) { '_' } else { c }).collect();
    let base = jscompat::trim(&sanitized);
    let base = if base.is_empty() { "image" } else { base };
    let mut per_page: HashMap<u64, usize> = HashMap::new();
    let mut out = Vec::with_capacity(images.len());
    for (i, image) in images.iter().enumerate() {
        if cancel.is_cancelled() { return Err(JobFailure::new("cancelled", None)); }
        let number = per_page.entry(image.page.to_bits()).or_insert(0);
        *number += 1;
        let stem = format!("{}-p{}-{}", base, jscompat::number_to_string(image.page), number);
        let mut suffix = 1u64;
        let file = loop {
            let name = if suffix == 1 { format!("{stem}.png") } else { format!("{stem} ({suffix}).png") };
            let path = host::join(&[out_dir, &name]);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut opened) => {
                    if let Err(error) = opened.write_all(&image.png) {
                        drop(opened);
                        let _ = std::fs::remove_file(&path);
                        let message = main_core::node_fs::node_error(&error, "write", &path).message;
                        return Err(JobFailure::new("failed", Some(&message)));
                    }
                    break path;
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => suffix += 1,
                Err(error) => {
                    let message = main_core::node_fs::node_error(&error, "open", &path).message;
                    return Err(JobFailure::new("failed", Some(&message)));
                }
            }
        };
        out.push(IpcValue::Object(vec![
            ("page".into(), IpcValue::Number(image.page)), ("file".into(), IpcValue::String(file)),
            ("width".into(), IpcValue::Number(image.width)), ("height".into(), IpcValue::Number(image.height)),
        ]));
        progress((i + 1) as f64, images.len() as f64);
    }
    Ok(IpcValue::Array(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn keeps_existing_file_and_uses_next_name() {
        let dir = std::env::temp_dir().join(format!("pdf-image-output-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("unsafe_name-p2-1.png"), b"existing").unwrap();
        let image = ExtractedImage { page: 2.0, width: 10.0, height: 20.0, png: b"new".to_vec() };
        let result = write_images(dir.to_str().unwrap(), "unsafe/name", &[image], &|_, _| {}, &CancelToken::default()).unwrap();
        assert!(matches!(result, IpcValue::Array(items) if items.len() == 1));
        assert_eq!(std::fs::read(dir.join("unsafe_name-p2-1.png")).unwrap(), b"existing");
        assert_eq!(std::fs::read(dir.join("unsafe_name-p2-1 (2).png")).unwrap(), b"new");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
