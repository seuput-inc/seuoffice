//! Detect uniformly margined print-page content using visible PDF objects.
use super::geometry::{Rect, print_content_box};
use super::paths::{number, object, ptr};
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
use serde_json::json;
fn u32at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().expect("PDFium scratch"))
}
fn paint(s: &mut PdfiumSession, obj: u64, b: u64) -> Result<(bool, bool), PdfiumError> {
    if number(
        s,
        "FPDFPath_GetDrawMode",
        &[ptr(obj, 0), ptr(b, 0), ptr(b, 4)],
    )? == 0.0
    {
        return Ok((false, false));
    }
    let d = s.read(b, 0, 8)?;
    let fill = u32at(&d, 0) != 0;
    let stroke = u32at(&d, 4) != 0;
    let mut visible = false;
    let mut opaque = false;
    for (on, name) in [
        (fill, "FPDFPageObj_GetFillColor"),
        (stroke, "FPDFPageObj_GetStrokeColor"),
    ] {
        if on
            && number(
                s,
                name,
                &[ptr(obj, 0), ptr(b, 0), ptr(b, 4), ptr(b, 8), ptr(b, 12)],
            )? != 0.0
        {
            let alpha = u32at(&s.read(b, 12, 4)?, 0);
            visible |= alpha > 0;
            if name == "FPDFPageObj_GetFillColor" {
                opaque = alpha >= 128;
            }
        }
    }
    Ok((visible, opaque))
}
pub(super) fn detect(
    s: &mut PdfiumSession,
    page: u64,
    width: f64,
    height: f64,
) -> Result<Option<Rect>, PdfiumError> {
    let buf = s.alloc(16)?;
    let result = (|| {
        let count = number(s, "FPDFPage_CountObjects", &[ptr(page, 0)])? as i32;
        let mut boxes = Vec::new();
        let mut fills = Vec::new();
        for i in 0..count {
            let Some(obj) = object(s, "FPDFPage_GetObject", &[ptr(page, 0), json!(i)])? else {
                continue;
            };
            let kind = number(s, "FPDFPageObj_GetType", &[ptr(obj, 0)])? as i32;
            let (visible, opaque) = if kind == 2 {
                paint(s, obj, buf)?
            } else {
                (true, false)
            };
            if !visible
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
                f32::from_le_bytes(b[i..i + 4].try_into().expect("object bounds")) as f64
            };
            let rect = Rect {
                x0: f(0),
                y0: f(4),
                x1: f(8),
                y1: f(12),
            };
            if rect.width() <= 0.0 || rect.height() <= 0.0 {
                continue;
            }
            boxes.push(rect);
            if opaque {
                fills.push(rect);
            }
        }
        Ok(fills
            .into_iter()
            .find_map(|r| print_content_box(r, &boxes, width, height)))
    })();
    let free = s.free(buf);
    match (result, free) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(())) => Ok(v),
    }
}
