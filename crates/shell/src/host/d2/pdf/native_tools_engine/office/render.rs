//! PDFium whole-page and clipped-region rasterization.
use super::bitmap;
use super::geometry::Rect;
use super::ir::PageRender;
use super::paths::{number, object, ptr};
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
use serde_json::json;
pub(super) fn page(
    s: &mut PdfiumSession,
    page: u64,
    region: Option<Rect>,
    scale: f64,
) -> Result<Option<PageRender>, PdfiumError> {
    let pw = number(s, "FPDF_GetPageWidthF", &[ptr(page, 0)])?;
    let ph = number(s, "FPDF_GetPageHeightF", &[ptr(page, 0)])?;
    let r = region.unwrap_or(Rect {
        x0: 0.0,
        y0: 0.0,
        x1: pw,
        y1: ph,
    });
    let w = jscompat::math_round((r.x1 - r.x0) * scale).max(1.0);
    let h = jscompat::math_round((r.y1 - r.y0) * scale).max(1.0);
    let Some(bitmap_id) = object(s, "FPDFBitmap_Create", &[json!(w), json!(h), json!(0)])? else {
        return Ok(None);
    };
    let result = (|| {
        number(
            s,
            "FPDFBitmap_FillRect",
            &[
                ptr(bitmap_id, 0),
                json!(0),
                json!(0),
                json!(w),
                json!(h),
                json!(u32::MAX),
            ],
        )?;
        s.call(
            "FPDF_RenderPageBitmap",
            &[
                ptr(bitmap_id, 0),
                ptr(page, 0),
                json!(jscompat::math_round(-r.x0 * scale)),
                json!(jscompat::math_round(-(ph - r.y1) * scale)),
                json!(jscompat::math_round(pw * scale)),
                json!(jscompat::math_round(ph * scale)),
                json!(0),
                json!(0),
            ],
        )?;
        let Some(rgba) = bitmap::bitmap_to_rgba(s, bitmap_id)? else {
            return Ok(None);
        };
        let data = bitmap::encode_png(&rgba).map_err(PdfiumError::from)?;
        Ok(Some(PageRender {
            data,
            mime: "image/png".into(),
            pixel_width: rgba.width as u32,
            pixel_height: rgba.height as u32,
        }))
    })();
    let close = s.call("FPDFBitmap_Destroy", &[ptr(bitmap_id, 0)]);
    match (result, close) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(_)) => Ok(v),
    }
}
pub(super) fn by_index(
    s: &mut PdfiumSession,
    doc: u64,
    index: usize,
    scale: f64,
) -> Result<Option<PageRender>, PdfiumError> {
    let Some(p) = object(s, "FPDF_LoadPage", &[ptr(doc, 0), json!(index)])? else {
        return Ok(None);
    };
    let result = page(s, p, None, scale);
    let close = s.call("FPDF_ClosePage", &[ptr(p, 0)]);
    match (result, close) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(_)) => Ok(v),
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    Empty,
    Text,
    Paint,
    Mixed,
}
fn profile(
    s: &mut PdfiumSession,
    form: u64,
    depth: usize,
    scratch: u64,
    forms: bool,
) -> Result<Profile, PdfiumError> {
    if depth > 3 || !forms {
        return Ok(Profile::Mixed);
    }
    let mut text = false;
    let mut paint = false;
    let count = number(s, "FPDFFormObj_CountObjects", &[ptr(form, 0)])? as i32;
    for i in 0..count {
        let Some(child) = object(s, "FPDFFormObj_GetObject", &[ptr(form, 0), json!(i)])? else {
            continue;
        };
        let kind = number(s, "FPDFPageObj_GetType", &[ptr(child, 0)])? as i32;
        match kind {
            1 => text = true,
            5 => match profile(s, child, depth + 1, scratch, forms)? {
                Profile::Mixed => return Ok(Profile::Mixed),
                Profile::Text => text = true,
                Profile::Paint => paint = true,
                Profile::Empty => {}
            },
            2 => {
                if number(
                    s,
                    "FPDFPath_GetDrawMode",
                    &[ptr(child, 0), ptr(scratch, 0), ptr(scratch, 4)],
                )? != 0.0
                {
                    let b = s.read(scratch, 0, 8)?;
                    let fill = u32::from_le_bytes(b[..4].try_into().expect("draw mode")) != 0;
                    let stroke = u32::from_le_bytes(b[4..8].try_into().expect("draw mode")) != 0;
                    for (use_it, name) in [
                        (fill, "FPDFPageObj_GetFillColor"),
                        (stroke, "FPDFPageObj_GetStrokeColor"),
                    ] {
                        if use_it
                            && number(
                                s,
                                name,
                                &[
                                    ptr(child, 0),
                                    ptr(scratch, 0),
                                    ptr(scratch, 4),
                                    ptr(scratch, 8),
                                    ptr(scratch, 12),
                                ],
                            )? != 0.0
                        {
                            let alpha = s.read(scratch, 12, 4)?;
                            if u32::from_le_bytes(alpha[..4].try_into().expect("color alpha")) > 0 {
                                paint = true;
                                break;
                            }
                        }
                    }
                }
            }
            _ => paint = true,
        }
        if text && paint {
            return Ok(Profile::Mixed);
        }
    }
    Ok(if text {
        Profile::Text
    } else if paint {
        Profile::Paint
    } else {
        Profile::Empty
    })
}
pub(super) fn without_text(
    s: &mut PdfiumSession,
    doc: u64,
    index: usize,
    scale: f64,
) -> Result<Option<PageRender>, PdfiumError> {
    let Some(p) = object(s, "FPDF_LoadPage", &[ptr(doc, 0), json!(index)])? else {
        return Ok(None);
    };
    let result = (|| {
        let forms =
            s.supports("FPDFFormObj_CountObjects")? && s.supports("FPDFFormObj_GetObject")?;
        let scratch = s.alloc(16)?;
        let work = (|| {
            let count = number(s, "FPDFPage_CountObjects", &[ptr(p, 0)])? as i32;
            for i in (0..count).rev() {
                let Some(obj) = object(s, "FPDFPage_GetObject", &[ptr(p, 0), json!(i)])? else {
                    continue;
                };
                let kind = number(s, "FPDFPageObj_GetType", &[ptr(obj, 0)])? as i32;
                let strip = if kind == 1 {
                    true
                } else if kind == 5 {
                    match profile(s, obj, 0, scratch, forms)? {
                        Profile::Mixed => return Ok(None),
                        Profile::Text => true,
                        _ => false,
                    }
                } else {
                    false
                };
                if strip && number(s, "FPDFPage_RemoveObject", &[ptr(p, 0), ptr(obj, 0)])? != 0.0 {
                    s.call("FPDFPageObj_Destroy", &[ptr(obj, 0)])?;
                }
            }
            page(s, p, None, scale)
        })();
        let free = s.free(scratch);
        match (work, free) {
            (Err(e), _) | (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(())) => Ok(v),
        }
    })();
    let close = s.call("FPDF_ClosePage", &[ptr(p, 0)]);
    match (result, close) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(_)) => Ok(v),
    }
}
