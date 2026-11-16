//! Extract page images through nested forms, clipping and inherited alpha.
use super::geometry::Rect;
use super::image_payload::{self, Crop};
use super::ir::ImageBlock;
use super::matrix::{Matrix, intersection};
use super::paths::{clip_box, matrix, number, object, ptr};
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
use serde_json::json;

fn alpha(s: &mut PdfiumSession, obj: u64, scratch: u64) -> Result<Option<u32>, PdfiumError> {
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
        return Ok(None);
    }
    let bytes = s.read(scratch, 12, 4)?;
    Ok(Some(u32::from_le_bytes(
        bytes[..4].try_into().expect("fill alpha"),
    )))
}
#[allow(clippy::too_many_arguments)]
fn visit(
    s: &mut PdfiumSession,
    doc: u64,
    page: u64,
    obj: u64,
    parent: Option<Matrix>,
    depth: usize,
    z: usize,
    mut clip: Option<Rect>,
    inherited: u32,
    rotation: i32,
    f: u64,
    u: u64,
    clips: bool,
    forms: bool,
    skip: &dyn Fn(Rect) -> bool,
    out: &mut Vec<ImageBlock>,
) -> Result<(), PdfiumError> {
    let kind = number(s, "FPDFPageObj_GetType", &[ptr(obj, 0)])? as i32;
    if let Some(own) = if clips { clip_box(s, obj, f)? } else { None } {
        let mapped = parent.unwrap_or(Matrix::IDENTITY).rect(own);
        clip = Some(if let Some(old) = clip {
            let Some(overlap) = intersection(old, mapped) else {
                return Ok(());
            };
            overlap
        } else {
            mapped
        });
    }
    if kind == 5 {
        if depth >= 4 || !forms {
            return Ok(());
        }
        let frame = parent
            .unwrap_or(Matrix::IDENTITY)
            .compose(matrix(s, obj, f)?);
        let a = alpha(s, obj, u)?.unwrap_or(inherited);
        let count = number(s, "FPDFFormObj_CountObjects", &[ptr(obj, 0)])? as i32;
        for i in 0..count {
            if let Some(child) = object(s, "FPDFFormObj_GetObject", &[ptr(obj, 0), json!(i)])? {
                visit(
                    s,
                    doc,
                    page,
                    child,
                    Some(frame),
                    depth + 1,
                    z,
                    clip,
                    a,
                    rotation,
                    f,
                    u,
                    clips,
                    forms,
                    skip,
                    out,
                )?;
            }
        }
        return Ok(());
    }
    if kind != 3 {
        return Ok(());
    }
    let own = matrix(s, obj, f)?;
    let composed = parent.map_or(own, |p| p.compose(own));
    let mirror = parent.map_or((false, false), |_| own.mirror_between(composed));
    let bounds = if parent.is_none() {
        if number(
            s,
            "FPDFPageObj_GetBounds",
            &[ptr(obj, 0), ptr(f, 0), ptr(f, 4), ptr(f, 8), ptr(f, 12)],
        )? == 0.0
        {
            return Ok(());
        }
        let b = s.read(f, 0, 16)?;
        let n = |i: usize| {
            f32::from_le_bytes(b[i..i + 4].try_into().expect("object bounds")) as f64
        };
        Rect {
            x0: n(0),
            y0: n(4),
            x1: n(8),
            y1: n(12),
        }
    } else {
        composed.rect(Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 1.0,
            y1: 1.0,
        })
    };
    if bounds.x1 <= bounds.x0 || bounds.y1 <= bounds.y0 {
        return Ok(());
    }
    let a = alpha(s, obj, u)?.unwrap_or(inherited);
    if a <= 20 {
        return Ok(());
    }
    let crop_box = if let Some(clip) = clip {
        let Some(visible) = intersection(bounds, clip) else {
            return Ok(());
        };
        let shrinks = visible.x0 - bounds.x0 > 1.0
            || visible.y0 - bounds.y0 > 1.0
            || bounds.x1 - visible.x1 > 1.0
            || bounds.y1 - visible.y1 > 1.0;
        if shrinks && composed.aligned() {
            Some(visible)
        } else {
            None
        }
    } else {
        None
    };
    if skip(crop_box.unwrap_or(bounds)) {
        return Ok(());
    }
    let crop = crop_box.map(|b| Crop {
        left: (b.x0 - bounds.x0) / (bounds.x1 - bounds.x0),
        right: (bounds.x1 - b.x1) / (bounds.x1 - bounds.x0),
        top: (bounds.y1 - b.y1) / (bounds.y1 - bounds.y0),
        bottom: (b.y0 - bounds.y0) / (bounds.y1 - bounds.y0),
    });
    if let Some(mut image) =
        image_payload::extract(s, doc, page, obj, bounds, crop, a, rotation, mirror)?
    {
        image.bounds = crop_box.unwrap_or(bounds);
        image.z = Some(z as i64);
        out.push(image);
    }
    Ok(())
}
pub(super) fn read_images(
    s: &mut PdfiumSession,
    doc: u64,
    page: u64,
    skip_below: usize,
    rotation: i32,
    skip: &dyn Fn(Rect) -> bool,
) -> Result<Vec<ImageBlock>, PdfiumError> {
    let f = s.alloc(24)?;
    let u = match s.alloc(16) {
        Ok(v) => v,
        Err(e) => {
            let _ = s.free(f);
            return Err(e);
        }
    };
    let result = (|| {
        let clips = s.supports("FPDFPageObj_GetClipPath")?
            && s.supports("FPDFClipPath_CountPaths")?
            && s.supports("FPDFClipPath_CountPathSegments")?
            && s.supports("FPDFClipPath_GetPathSegment")?;
        let forms =
            s.supports("FPDFFormObj_CountObjects")? && s.supports("FPDFFormObj_GetObject")?;
        let count = number(s, "FPDFPage_CountObjects", &[ptr(page, 0)])? as usize;
        let mut out = Vec::new();
        for i in skip_below..count {
            if let Some(obj) = object(s, "FPDFPage_GetObject", &[ptr(page, 0), json!(i)])? {
                visit(
                    s, doc, page, obj, None, 0, i, None, 255, rotation, f, u, clips, forms, skip,
                    &mut out,
                )?;
            }
        }
        Ok(out)
    })();
    let free_f = s.free(f);
    let free_u = s.free(u);
    match (result, free_f, free_u) {
        (Err(e), _, _) | (Ok(_), Err(e), _) | (Ok(_), Ok(()), Err(e)) => Err(e),
        (Ok(v), Ok(()), Ok(())) => Ok(v),
    }
}
