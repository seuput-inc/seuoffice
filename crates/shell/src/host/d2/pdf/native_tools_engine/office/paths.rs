//! Page path and clip extraction, including nested form transforms.
use super::geometry::Rect;
use super::ir::{RawPath, RawSubpath};
use super::matrix::{Matrix, intersection};
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession, read_number};
use serde_json::{Value, json};

pub(super) fn ptr(id: u64, offset: usize) -> Value {
    json!({"ref":id,"offset":offset})
}
pub(super) fn number(
    s: &mut PdfiumSession,
    name: &str,
    args: &[Value],
) -> Result<f64, PdfiumError> {
    read_number(&s.call(name, args)?)
        .ok_or_else(|| PdfiumError::from(format!("invalid {name} result")))
}
pub(super) fn object(
    s: &mut PdfiumSession,
    name: &str,
    args: &[Value],
) -> Result<Option<u64>, PdfiumError> {
    let v = s.call(name, args)?;
    if v.as_u64() == Some(0) {
        Ok(None)
    } else {
        v.get("ref")
            .and_then(Value::as_u64)
            .map(Some)
            .ok_or_else(|| PdfiumError::from(format!("invalid {name} reference")))
    }
}
fn u32at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().expect("PDFium scratch buffer"))
}
fn f32at(b: &[u8], i: usize) -> f64 {
    f32::from_le_bytes(b[i..i + 4].try_into().expect("PDFium scratch buffer")) as f64
}
pub(super) fn matrix(s: &mut PdfiumSession, obj: u64, scratch: u64) -> Result<Matrix, PdfiumError> {
    if number(s, "FPDFPageObj_GetMatrix", &[ptr(obj, 0), ptr(scratch, 0)])? == 0.0 {
        return Ok(Matrix::IDENTITY);
    }
    let b = s.read(scratch, 0, 24)?;
    Ok(Matrix([
        f32at(&b, 0),
        f32at(&b, 4),
        f32at(&b, 8),
        f32at(&b, 12),
        f32at(&b, 16),
        f32at(&b, 20),
    ]))
}
pub(super) fn clip_box(
    s: &mut PdfiumSession,
    obj: u64,
    scratch: u64,
) -> Result<Option<Rect>, PdfiumError> {
    let Some(clip) = object(s, "FPDFPageObj_GetClipPath", &[ptr(obj, 0)])? else {
        return Ok(None);
    };
    let count = number(s, "FPDFClipPath_CountPaths", &[ptr(clip, 0)])? as i32;
    if count <= 0 {
        return Ok(None);
    }
    let mut bounds = None;
    for p in 0..count {
        let segments = number(
            s,
            "FPDFClipPath_CountPathSegments",
            &[ptr(clip, 0), json!(p)],
        )? as i32;
        if segments <= 0 {
            return Ok(None);
        }
        let mut r = Rect {
            x0: f64::INFINITY,
            y0: f64::INFINITY,
            x1: f64::NEG_INFINITY,
            y1: f64::NEG_INFINITY,
        };
        for n in 0..segments {
            let Some(seg) = object(
                s,
                "FPDFClipPath_GetPathSegment",
                &[ptr(clip, 0), json!(p), json!(n)],
            )?
            else {
                continue;
            };
            if number(
                s,
                "FPDFPathSegment_GetPoint",
                &[ptr(seg, 0), ptr(scratch, 0), ptr(scratch, 4)],
            )? == 0.0
            {
                continue;
            }
            let b = s.read(scratch, 0, 8)?;
            let (x, y) = (f32at(&b, 0), f32at(&b, 4));
            r.x0 = r.x0.min(x);
            r.y0 = r.y0.min(y);
            r.x1 = r.x1.max(x);
            r.y1 = r.y1.max(y);
        }
        if r.x0 > r.x1 {
            return Ok(None);
        }
        bounds = Some(match bounds {
            None => r,
            Some(prev) => match intersection(prev, r) {
                Some(v) => v,
                None => {
                    return Ok(Some(Rect {
                        x0: 0.0,
                        y0: 0.0,
                        x1: 0.0,
                        y1: 0.0,
                    }));
                }
            },
        });
    }
    Ok(bounds)
}
pub(super) fn color(
    s: &mut PdfiumSession,
    name: &str,
    obj: u64,
    scratch: u64,
) -> Result<(String, u32), PdfiumError> {
    if number(
        s,
        name,
        &[
            ptr(obj, 0),
            ptr(scratch, 0),
            ptr(scratch, 4),
            ptr(scratch, 8),
            ptr(scratch, 12),
        ],
    )? == 0.0
    {
        return Ok(("000000".into(), 255));
    }
    let b = s.read(scratch, 0, 16)?;
    Ok((
        format!(
            "{:02X}{:02X}{:02X}",
            u32at(&b, 0),
            u32at(&b, 4),
            u32at(&b, 8)
        ),
        u32at(&b, 12),
    ))
}
#[allow(clippy::too_many_arguments)]
fn visit(
    s: &mut PdfiumSession,
    obj: u64,
    parent: Matrix,
    depth: usize,
    z: usize,
    mut clip: Option<Rect>,
    f: u64,
    u: u64,
    clips: bool,
    forms: bool,
    out: &mut Vec<RawPath>,
) -> Result<(), PdfiumError> {
    let kind = number(s, "FPDFPageObj_GetType", &[ptr(obj, 0)])? as i32;
    if let Some(own) = if clips { clip_box(s, obj, f)? } else { None } {
        let mapped = parent.rect(own);
        clip = Some(if let Some(old) = clip {
            let Some(intersect) = intersection(old, mapped) else {
                return Ok(());
            };
            intersect
        } else {
            mapped
        });
    }
    if kind == 5 {
        if depth >= 4 || !forms {
            return Ok(());
        }
        let form = parent.compose(matrix(s, obj, f)?);
        let children = number(s, "FPDFFormObj_CountObjects", &[ptr(obj, 0)])? as i32;
        for i in 0..children {
            if let Some(child) = object(s, "FPDFFormObj_GetObject", &[ptr(obj, 0), json!(i)])? {
                visit(s, child, form, depth + 1, z, clip, f, u, clips, forms, out)?;
            }
        }
        return Ok(());
    }
    if kind != 2 {
        return Ok(());
    }
    let drawn = number(
        s,
        "FPDFPath_GetDrawMode",
        &[ptr(obj, 0), ptr(u, 0), ptr(u, 4)],
    )? != 0.0;
    let (filled, stroked) = if drawn {
        let b = s.read(u, 0, 8)?;
        (u32at(&b, 0) != 0, u32at(&b, 4) != 0)
    } else {
        (false, false)
    };
    if !filled && !stroked {
        return Ok(());
    }
    let transform = parent.compose(matrix(s, obj, f)?);
    let (fill_color, fill_alpha) = color(s, "FPDFPageObj_GetFillColor", obj, u)?;
    let (stroke_color, _) = color(s, "FPDFPageObj_GetStrokeColor", obj, u)?;
    let mut stroke_width =
        if number(s, "FPDFPageObj_GetStrokeWidth", &[ptr(obj, 0), ptr(f, 0)])? != 0.0 {
            f32at(&s.read(f, 0, 4)?, 0)
        } else {
            1.0
        };
    let [a, b, c, d, _, _] = transform.0;
    let scale = (a * d - b * c).abs().sqrt();
    stroke_width *= if scale == 0.0 { 1.0 } else { scale };
    let mut paths: Vec<RawSubpath> = Vec::new();
    let count = number(s, "FPDFPath_CountSegments", &[ptr(obj, 0)])? as i32;
    for i in 0..count {
        let Some(seg) = object(s, "FPDFPath_GetPathSegment", &[ptr(obj, 0), json!(i)])? else {
            continue;
        };
        let kind = number(s, "FPDFPathSegment_GetType", &[ptr(seg, 0)])? as i32;
        if number(
            s,
            "FPDFPathSegment_GetPoint",
            &[ptr(seg, 0), ptr(f, 0), ptr(f, 4)],
        )? == 0.0
        {
            continue;
        }
        let bytes = s.read(f, 0, 8)?;
        let point = transform.point(f32at(&bytes, 0), f32at(&bytes, 4));
        if kind == 2 || paths.is_empty() {
            paths.push(RawSubpath {
                points: vec![point],
                closed: false,
                has_curves: false,
                line_to: Some(vec![false]),
            });
        } else if let Some(current) = paths.last_mut() {
            current.points.push(point);
            if let Some(lines) = current.line_to.as_mut() {
                lines.push(kind != 1 && kind != 2);
            }
            if kind == 1 {
                current.has_curves = true;
            }
        }
        if number(s, "FPDFPathSegment_GetClose", &[ptr(seg, 0)])? != 0.0 {
            if let Some(current) = paths.last_mut() {
                current.closed = true;
            }
        }
    }
    if !paths.is_empty() {
        out.push(RawPath {
            subpaths: paths,
            filled,
            stroked,
            fill_color,
            stroke_color,
            stroke_width,
            fill_alpha: Some(fill_alpha as u8),
            from_form: depth > 0,
            z: Some(z as i64),
            clip_box: clip,
        });
    }
    Ok(())
}
pub(super) fn read_paths(
    s: &mut PdfiumSession,
    page: u64,
    skip: usize,
) -> Result<Vec<RawPath>, PdfiumError> {
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
        for i in skip..count {
            if let Some(obj) = object(s, "FPDFPage_GetObject", &[ptr(page, 0), json!(i)])? {
                visit(
                    s,
                    obj,
                    Matrix::IDENTITY,
                    0,
                    i,
                    None,
                    f,
                    u,
                    clips,
                    forms,
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
