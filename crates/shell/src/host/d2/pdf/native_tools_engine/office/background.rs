//! Detect and rasterize page-covering background stacks.
use super::geometry::Rect;
use super::ir::{PageRender, PdfChar};
use super::paths::{number, object, ptr};
use super::{image_payload, render};
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
use serde_json::json;
use std::collections::HashSet;
fn u32at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().expect("PDFium scratch"))
}
fn draw(s: &mut PdfiumSession, obj: u64, b: u64) -> Result<(bool, bool, bool), PdfiumError> {
    if number(
        s,
        "FPDFPath_GetDrawMode",
        &[ptr(obj, 0), ptr(b, 0), ptr(b, 4)],
    )? == 0.0
    {
        return Ok((false, false, false));
    }
    let data = s.read(b, 0, 8)?;
    let (fill, stroke) = (u32at(&data, 0) != 0, u32at(&data, 4) != 0);
    let (mut visible, mut opaque, mut colored) = (false, false, false);
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
            let d = s.read(b, 0, 16)?;
            visible |= u32at(&d, 12) > 0;
            if name == "FPDFPageObj_GetFillColor" {
                opaque = u32at(&d, 12) >= 128;
                colored = opaque && (0..3).any(|i| u32at(&d, i * 4) < 0xf2);
            }
        }
    }
    Ok((visible, opaque, colored))
}
fn form(
    s: &mut PdfiumSession,
    obj: u64,
    depth: usize,
    forms: bool,
) -> Result<Option<Vec<u64>>, PdfiumError> {
    if depth > 3 || !forms {
        return Ok(None);
    }
    let n = number(s, "FPDFFormObj_CountObjects", &[ptr(obj, 0)])? as i32;
    if n == 0 || n > 64 {
        return Ok(None);
    }
    let mut texts = Vec::new();
    let mut paint = false;
    for i in 0..n {
        let Some(c) = object(s, "FPDFFormObj_GetObject", &[ptr(obj, 0), json!(i)])? else {
            return Ok(None);
        };
        match number(s, "FPDFPageObj_GetType", &[ptr(c, 0)])? as i32 {
            1 => texts.push(c),
            5 => {
                let Some(sub) = form(s, c, depth + 1, forms)? else {
                    return Ok(None);
                };
                paint = true;
                texts.extend(sub);
            }
            _ => paint = true,
        }
        if texts.len() > 10 {
            return Ok(None);
        }
    }
    Ok(paint.then_some(texts))
}
pub(super) struct Stack {
    pub count: usize,
    pub raster: bool,
    pub suspect_only: bool,
    pub texts: HashSet<u64>,
}
pub(super) fn detect(
    s: &mut PdfiumSession,
    doc: u64,
    page: u64,
    width: f64,
    height: f64,
    content: Option<Rect>,
) -> Result<Stack, PdfiumError> {
    let b = s.alloc(16)?;
    let result = (|| {
        let forms =
            s.supports("FPDFFormObj_CountObjects")? && s.supports("FPDFFormObj_GetObject")?;
        let total = number(s, "FPDFPage_CountObjects", &[ptr(page, 0)])? as usize;
        let (mut count, mut raster, mut sure, mut prefix) = (0, false, false, true);
        let mut texts = HashSet::new();
        let page_rect = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: width,
            y1: height,
        };
        for i in 0..total {
            let Some(obj) = object(s, "FPDFPage_GetObject", &[ptr(page, 0), json!(i)])? else {
                prefix = false;
                continue;
            };
            let kind = number(s, "FPDFPageObj_GetType", &[ptr(obj, 0)])? as i32;
            let (visible, opaque, colored) = if kind == 2 {
                draw(s, obj, b)?
            } else {
                (true, false, false)
            };
            if !visible {
                continue;
            }
            let form_text = if kind == 5 && prefix {
                form(s, obj, 0, forms)?
            } else {
                None
            };
            if !matches!(kind, 2 | 3 | 4) && form_text.is_none() {
                prefix = false;
                continue;
            }
            if number(
                s,
                "FPDFPageObj_GetBounds",
                &[ptr(obj, 0), ptr(b, 0), ptr(b, 4), ptr(b, 8), ptr(b, 12)],
            )? == 0.0
            {
                prefix = false;
                continue;
            }
            let data = s.read(b, 0, 16)?;
            let f = |i: usize| {
                f32::from_le_bytes(data[i..i + 4].try_into().expect("object bounds")) as f64
            };
            let bounds = Rect {
                x0: f(0),
                y0: f(4),
                x1: f(8),
                y1: f(12),
            };
            let ratio = if prefix { 0.9 } else { 0.96 };
            if !bounds.covers(page_rect, ratio) && !content.is_some_and(|r| bounds.covers(r, ratio))
            {
                prefix = false;
                continue;
            }
            if !prefix && kind == 3 {
                if let Some(px) = image_payload::rendered(s, doc, page, obj, 1.0)? {
                    let mut sampled = 0;
                    let mut transparent = 0;
                    for a in px.pixels.iter().skip(3).step_by(16) {
                        sampled += 1;
                        if *a < 250 {
                            transparent += 1;
                        }
                    }
                    if sampled > 0 && transparent as f64 / sampled as f64 > 0.5 {
                        continue;
                    }
                }
            }
            let certain = kind != 2 || colored;
            let suspect = !certain && prefix && opaque;
            let candidate = certain || suspect;
            if prefix || candidate || raster {
                count = i + 1;
            }
            if candidate {
                raster = true;
                sure |= certain;
                if let Some(handles) = form_text {
                    texts.extend(handles);
                }
            }
        }
        Ok(Stack {
            count,
            raster,
            suspect_only: !sure,
            texts,
        })
    })();
    let free = s.free(b);
    match (result, free) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(())) => Ok(v),
    }
}
fn near_white(pixels: &[u8]) -> bool {
    pixels
        .chunks(16)
        .all(|p| p.len() >= 3 && p[0] >= 0xf2 && p[1] >= 0xf2 && p[2] >= 0xf2)
}
pub(super) fn bake(
    s: &mut PdfiumSession,
    doc: u64,
    index: usize,
    count: usize,
) -> Result<Option<PageRender>, PdfiumError> {
    let Some(page) = object(s, "FPDF_LoadPage", &[ptr(doc, 0), json!(index)])? else {
        return Ok(None);
    };
    let result = (|| {
        let total = number(s, "FPDFPage_CountObjects", &[ptr(page, 0)])? as usize;
        for i in (count..total).rev() {
            if let Some(obj) = object(s, "FPDFPage_GetObject", &[ptr(page, 0), json!(i)])? {
                if number(s, "FPDFPage_RemoveObject", &[ptr(page, 0), ptr(obj, 0)])? != 0.0 {
                    s.call("FPDFPageObj_Destroy", &[ptr(obj, 0)])?;
                }
            }
        }
        let Some(rendered) = render::page(s, page, None, 1.5)? else {
            return Ok(None);
        };
        let image = image::load_from_memory(&rendered.data)
            .map_err(|e| PdfiumError::from(e.to_string()))?
            .to_rgba8();
        if near_white(image.as_raw()) {
            return Ok(None);
        }
        Ok(Some(rendered))
    })();
    let close = s.call("FPDF_ClosePage", &[ptr(page, 0)]);
    match (result, close) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(_)) => Ok(v),
    }
}

pub(super) fn remove_buried(
    s: &mut PdfiumSession,
    page: u64,
    stack: &Stack,
    chars: &mut Vec<PdfChar>,
) -> Result<(), PdfiumError> {
    let mut buried = stack.texts.clone();
    for i in 0..stack.count {
        if let Some(obj) = object(s, "FPDFPage_GetObject", &[ptr(page, 0), json!(i)])? {
            if number(s, "FPDFPageObj_GetType", &[ptr(obj, 0)])? == 1.0 {
                buried.insert(obj);
            }
        }
    }
    if buried.is_empty() {
        return Ok(());
    }
    let Some(tp) = object(s, "FPDFText_LoadPage", &[ptr(page, 0)])? else {
        return Ok(());
    };
    let result = (|| {
        let mut keep = Vec::with_capacity(chars.len());
        for c in chars.drain(..) {
            let remove = if let Some(i) = c.pdf_char_index {
                object(s, "FPDFText_GetTextObject", &[ptr(tp, 0), json!(i)])?
                    .is_some_and(|id| buried.contains(&id))
            } else {
                false
            };
            if !remove {
                keep.push(c);
            }
        }
        *chars = keep;
        Ok(())
    })();
    let close = s.call("FPDFText_ClosePage", &[ptr(tp, 0)]);
    match (result, close) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(_)) => Ok(v),
    }
}
#[cfg(test)]
mod tests {
    use super::near_white;

    #[test]
    fn background_probe_samples_last_partial_group() {
        assert!(near_white(&[255, 255, 255, 255]));
        assert!(!near_white(&[0, 255, 255, 255]));
        let mut pixels = vec![255; 20];
        pixels[16] = 241;
        assert!(!near_white(&pixels));
        pixels[16] = 242;
        assert!(near_white(&pixels));
    }
}
