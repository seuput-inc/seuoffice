//! The Word editor's captured and copied images.

use std::sync::Arc;

use clipboard_rs::common::RustImage;
use clipboard_rs::{Clipboard as _, ClipboardContent, ClipboardContext, RustImageData};
use shell_contract::ViewId;
use shell_contract::runtime::Thread;

use crate::host::contents::capture_png;
use crate::host::{on_ui, window::Windows};
use crate::native::{CaptureSpec, Rect};

pub(super) fn capture(
    windows: &Windows, view: ViewId, x: f64, y: f64, width: f64, height: f64,
) -> Result<Option<Vec<u8>>, String> {
    let webview = windows.webview(view).ok_or_else(|| format!("no view {}", view.0))?;
    let spec = CaptureSpec { clip: Some(Rect { x, y, width, height }), ..CaptureSpec::default() };
    let bytes = capture_png(&webview, spec)?;
    if bytes.is_empty() { Ok(None) } else { Ok(Some(bytes)) }
}

pub(super) fn size(bytes: &[u8]) -> Option<(f64, f64)> {
    image::load_from_memory(bytes).ok().map(|image| (f64::from(image.width()), f64::from(image.height())))
}

pub(super) fn write_clipboard(ui: &Arc<dyn Thread>, bytes: &[u8], html: &str) {
    let bytes = bytes.to_vec();
    let html = html.to_owned();
    on_ui(ui, move || {
        let result = ClipboardContext::new().and_then(|clipboard| {
            let image = RustImageData::from_bytes(&bytes)?;
            clipboard.set(vec![ClipboardContent::Image(image), ClipboardContent::Html(html)])
        });
        if let Err(error) = result { eprintln!("shell: clipboard.write: {error}"); }
    });
}
