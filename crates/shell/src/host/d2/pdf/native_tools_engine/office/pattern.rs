//! Probe PDFium pattern fallback fills and retain their true painted pixels.
use super::bitmap::{self, Rgba};
use super::geometry::Rect;
use super::ir::ImageBlock;
use super::paths::{number, object, ptr};
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
use serde_json::json;
use std::collections::HashSet;
fn u32at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().expect("PDFium scratch"))
}
fn alone(
    s: &mut PdfiumSession,
    doc: u64,
    page_index: usize,
    index: usize,
    r: Rect,
    scale: f64,
) -> Result<Option<Rgba>, PdfiumError> {
    let Some(page) = object(s, "FPDF_LoadPage", &[ptr(doc, 0), json!(page_index)])? else {
        return Ok(None);
    };
    let result = (|| {
        let count = number(s, "FPDFPage_CountObjects", &[ptr(page, 0)])? as usize;
        for i in (0..count).rev() {
            if i == index {
                continue;
            }
            if let Some(obj) = object(s, "FPDFPage_GetObject", &[ptr(page, 0), json!(i)])?
                && number(s, "FPDFPage_RemoveObject", &[ptr(page, 0), ptr(obj, 0)])? != 0.0
            {
                s.call("FPDFPageObj_Destroy", &[ptr(obj, 0)])?;
            }
        }
        let pw = number(s, "FPDF_GetPageWidthF", &[ptr(page, 0)])?;
        let ph = number(s, "FPDF_GetPageHeightF", &[ptr(page, 0)])?;
        let w = jscompat::math_round(r.width() * scale).max(1.0);
        let h = jscompat::math_round(r.height() * scale).max(1.0);
        let Some(b) = object(s, "FPDFBitmap_Create", &[json!(w), json!(h), json!(1)])? else {
            return Ok(None);
        };
        let rendered = (|| {
            number(
                s,
                "FPDFBitmap_FillRect",
                &[ptr(b, 0), json!(0), json!(0), json!(w), json!(h), json!(0)],
            )?;
            s.call(
                "FPDF_RenderPageBitmap",
                &[
                    ptr(b, 0),
                    ptr(page, 0),
                    json!(jscompat::math_round(-r.x0 * scale)),
                    json!(jscompat::math_round(-(ph - r.y1) * scale)),
                    json!(jscompat::math_round(pw * scale)),
                    json!(jscompat::math_round(ph * scale)),
                    json!(0),
                    json!(0),
                ],
            )?;
            bitmap::bitmap_to_rgba(s, b)
        })();
        let destroy = s.call("FPDFBitmap_Destroy", &[ptr(b, 0)]);
        match (rendered, destroy) {
            (Err(e), _) | (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(_)) => Ok(v),
        }
    })();
    let close = s.call("FPDF_ClosePage", &[ptr(page, 0)]);
    match (result, close) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(_)) => Ok(v),
    }
}
#[derive(Debug, PartialEq)]
enum Verdict {
    Flat,
    Pattern,
    Invisible,
}

fn probe_verdict(pixels: &[u8], channel: u32) -> Verdict {
    let mut sampled = 0;
    let mut covered = 0;
    let mut off = false;
    for p in pixels.chunks_exact(4) {
        sampled += 1;
        if p[3] < 8 {
            continue;
        }
        covered += 1;
        off |= (0..3).any(|k| (p[k] as i32 - channel as i32).abs() > 24);
    }
    if sampled == 0 || (covered as f64) / (sampled as f64) < 0.05 {
        Verdict::Invisible
    } else if off {
        Verdict::Pattern
    } else {
        Verdict::Flat
    }
}

pub(super) fn read(
    s: &mut PdfiumSession,
    doc: u64,
    index: usize,
    page: u64,
    width: f64,
    height: f64,
    skip: usize,
) -> Result<(Vec<ImageBlock>, HashSet<usize>), PdfiumError> {
    let scratch = s.alloc(16)?;
    let result = (|| {
        let total = number(s, "FPDFPage_CountObjects", &[ptr(page, 0)])? as usize;
        let mut candidates = Vec::new();
        for i in skip..total {
            let Some(obj) = object(s, "FPDFPage_GetObject", &[ptr(page, 0), json!(i)])? else {
                continue;
            };
            if number(s, "FPDFPageObj_GetType", &[ptr(obj, 0)])? != 2.0
                || number(
                    s,
                    "FPDFPath_GetDrawMode",
                    &[ptr(obj, 0), ptr(scratch, 0), ptr(scratch, 4)],
                )? == 0.0
                || u32at(&s.read(scratch, 0, 4)?, 0) == 0
            {
                continue;
            }
            if number(
                s,
                "FPDFPageObj_GetFillColor",
                &[
                    ptr(obj, 0),
                    ptr(scratch, 0),
                    ptr(scratch, 4),
                    ptr(scratch, 8),
                    ptr(scratch, 12),
                ],
            )? == 0.0
            {
                continue;
            }
            let c = s.read(scratch, 0, 16)?;
            let channel = u32at(&c, 0);
            if channel != u32at(&c, 4)
                || channel != u32at(&c, 8)
                || !matches!(channel, 0xff | 0xbf)
                || u32at(&c, 12) <= 20
            {
                continue;
            }
            if number(
                s,
                "FPDFPageObj_GetBounds",
                &[
                    ptr(obj, 0),
                    ptr(scratch, 0),
                    ptr(scratch, 4),
                    ptr(scratch, 8),
                    ptr(scratch, 12),
                ],
            )? == 0.0
            {
                continue;
            }
            let b = s.read(scratch, 0, 16)?;
            let f = |i: usize| {
                f32::from_le_bytes(b[i..i + 4].try_into().expect("pattern bounds")) as f64
            };
            let r = Rect {
                x0: f(0).max(0.0),
                y0: f(4).max(0.0),
                x1: f(8).min(width),
                y1: f(12).min(height),
            };
            if r.width() < 12.0 || r.height() < 12.0 || r.area() < 400.0 {
                continue;
            }
            candidates.push((i, r, channel));
        }
        candidates.sort_by(|a, b| b.1.area().total_cmp(&a.1.area()));
        let mut images = Vec::new();
        let mut consumed = HashSet::new();
        for (i, r, channel) in candidates.into_iter().take(48) {
            let Some(probe) = alone(s, doc, index, i, r, 24.0 / r.width().max(r.height()))? else {
                continue;
            };
            match probe_verdict(&probe.pixels, channel) {
                Verdict::Invisible => {
                    consumed.insert(i);
                    continue;
                }
                Verdict::Flat if channel != 0xbf => continue,
                _ => {}
            }
            let scale = 1.5_f64.min((4_000_000.0 / r.area()).sqrt());
            let Some(px) = alone(s, doc, index, i, r, scale)? else {
                continue;
            };
            let data = bitmap::encode_png(&px).map_err(PdfiumError::from)?;
            consumed.insert(i);
            images.push(ImageBlock {
                bounds: r,
                data,
                mime: "image/png".into(),
                float: None,
                spacing_before_pt: None,
                z: Some(i as i64),
                synthetic: true,
                card_id: None,
            });
        }
        Ok((images, consumed))
    })();
    let free = s.free(scratch);
    match (result, free) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(())) => Ok(v),
    }
}

#[cfg(test)]
mod tests {
    use super::{Verdict, probe_verdict};

    #[test]
    fn probes_visible_flat_pattern_and_soft_mask() {
        assert_eq!(probe_verdict(&[255, 255, 255, 8], 255), Verdict::Flat);
        assert_eq!(probe_verdict(&[230, 255, 255, 255], 255), Verdict::Pattern);
        assert_eq!(probe_verdict(&[255, 255, 255, 7], 255), Verdict::Invisible);
        let mut pixels = vec![0; 80];
        pixels[0..4].copy_from_slice(&[255, 255, 255, 255]);
        assert_eq!(probe_verdict(&pixels, 255), Verdict::Flat);
        pixels.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(probe_verdict(&pixels, 255), Verdict::Invisible);
    }
}
