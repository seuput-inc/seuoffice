//! Preserve the source object's outline and synthetic bold stroke.
use super::super::{PdfiumSession, buffer, call, integer, offset, reference};
use serde_json::json;

#[derive(Clone, Copy)]
pub(super) struct Stroke {
    pub mode: i64,
    pub color: [u8; 4],
    pub width: f64,
    pub same_as_fill: bool,
}
pub(super) fn color(
    s: &mut PdfiumSession,
    obj: u64,
    method: &str,
) -> Result<Option<[u8; 4]>, String> {
    buffer(s, 16, |s, b| {
        if integer(call(
            s,
            method,
            &[
                reference(obj),
                reference(b),
                offset(b, 4),
                offset(b, 8),
                offset(b, 12),
            ],
        )?)? == 0
        {
            return Ok(None);
        }
        let v = s.read(b, 0, 16).map_err(|e| e.to_string())?;
        Ok(Some([v[0], v[4], v[8], v[12]]))
    })
}
pub(super) fn read(s: &mut PdfiumSession, obj: u64) -> Result<Option<Stroke>, String> {
    let mode = integer(call(s, "FPDFTextObj_GetTextRenderMode", &[reference(obj)])?)?;
    if mode != 2 && mode != 6 {
        return Ok(None);
    }
    let Some(stroke) = color(s, obj, "FPDFPageObj_GetStrokeColor")? else {
        return Ok(None);
    };
    let width = buffer(s, 4, |s, b| {
        if integer(call(
            s,
            "FPDFPageObj_GetStrokeWidth",
            &[reference(obj), reference(b)],
        )?)? == 0
        {
            return Ok(0.0);
        }
        let v = s.read(b, 0, 4).map_err(|e| e.to_string())?;
        Ok(f64::from(f32::from_le_bytes(
            v.as_slice()
                .try_into()
                .map_err(|_| "invalid stroke width")?,
        )))
    })?;
    let same_as_fill =
        color(s, obj, "FPDFPageObj_GetFillColor")?.is_some_and(|fill| fill[..3] == stroke[..3]);
    Ok(Some(Stroke {
        mode,
        color: stroke,
        width,
        same_as_fill,
    }))
}
pub(super) fn apply(
    s: &mut PdfiumSession,
    obj: u64,
    color: [u8; 4],
    width: f64,
    mode: i64,
) -> Result<(), String> {
    call(
        s,
        "FPDFTextObj_SetTextRenderMode",
        &[reference(obj), json!(mode)],
    )?;
    call(
        s,
        "FPDFPageObj_SetStrokeColor",
        &[
            reference(obj),
            json!(color[0]),
            json!(color[1]),
            json!(color[2]),
            json!(color[3]),
        ],
    )?;
    call(
        s,
        "FPDFPageObj_SetStrokeWidth",
        &[reference(obj), json!(width)],
    )?;
    call(s, "FPDFPageObj_SetLineJoin", &[reference(obj), json!(1)])?;
    Ok(())
}
