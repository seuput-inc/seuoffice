//! Recover invisible OCR glyphs over page-covering scan images.
use super::ir::PdfChar;
use super::paths::{number, object, ptr};
use super::quality::whitespace;
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
use serde_json::json;
pub(super) fn image_stats(
    s: &mut PdfiumSession,
    page: u64,
    width: f64,
    height: f64,
) -> Result<(usize, i32), PdfiumError> {
    let area = width * height;
    if area <= 0.0 {
        return Ok((0, -1));
    }
    let buf = s.alloc(16)?;
    let result = (|| {
        let total = number(s, "FPDFPage_CountObjects", &[ptr(page, 0)])? as i32;
        let (mut count, mut top) = (0, -1);
        for i in 0..total {
            let Some(obj) = object(s, "FPDFPage_GetObject", &[ptr(page, 0), json!(i)])? else {
                continue;
            };
            if number(s, "FPDFPageObj_GetType", &[ptr(obj, 0)])? != 3.0
                || number(
                    s,
                    "FPDFPageObj_GetBounds",
                    &[
                        ptr(obj, 0),
                        ptr(buf, 0),
                        ptr(buf, 4),
                        ptr(buf, 8),
                        ptr(buf, 12),
                    ],
                )? == 0.0
            {
                continue;
            }
            let b = s.read(buf, 0, 16)?;
            let f = |i: usize| {
                f32::from_le_bytes(b[i..i + 4].try_into().expect("image bounds")) as f64
            };
            let w = f(8).min(width) - f(0).max(0.0);
            let h = f(12).min(height) - f(4).max(0.0);
            if w > 0.0 && h > 0.0 && w * h / area >= 0.8 {
                count += 1;
                top = i;
            }
        }
        Ok((count, top))
    })();
    let free = s.free(buf);
    match (result, free) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(())) => Ok(v),
    }
}
pub(super) fn recover(chars: &mut [PdfChar], covering_images: usize) -> bool {
    let body: Vec<_> = chars
        .iter()
        .filter(|c| !c.is_generated && !whitespace(c.code))
        .collect();
    if body.len() < 20
        || (body.iter().filter(|c| c.invisible).count() as f64) / (body.len() as f64) < 0.8
        || covering_images == 0
    {
        return false;
    }
    for c in chars {
        c.invisible = false;
        if whitespace(c.code) {
            continue;
        }
        let h = c.bounds.height();
        if c.font_size < 2.0 && h >= 2.0 {
            c.font_size = h / 0.72;
        } else if c.font_size >= 2.0 && h < 0.3 * c.font_size {
            c.bounds.y0 = c.origin_y - 0.2 * c.font_size;
            c.bounds.y1 = c.origin_y + 0.8 * c.font_size;
            c.loose_box = c.bounds;
        }
    }
    true
}
