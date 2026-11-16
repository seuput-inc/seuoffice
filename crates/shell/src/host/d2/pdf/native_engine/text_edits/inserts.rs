//! Searchable PDF text insertion, grouped by font and page.
use super::super::super::pdfium_session::wire_number;
use super::font;
use super::{
    PdfiumSession, buffer, call, field, handle, integer, number, page_index, reference, text,
};
use serde_json::json;
use shell_contract::{IpcValue as V, runtime::CancelToken};
use std::collections::HashMap;

struct Entry<'a> {
    index: usize,
    page: usize,
    input: &'a V,
}
struct Face {
    handle: u64,
    truetype: bool,
    bold: bool,
}
fn pair(value: Option<&V>) -> Option<[f64; 2]> {
    let V::Array(v) = value? else { return None };
    Some([number(v.first())?, number(v.get(1))?])
}
fn color(value: Option<&V>) -> Option<[u8; 3]> {
    let V::Array(v) = value? else { return None };
    let channel = |i: usize| {
        let n = number(v.get(i))?;
        (n.is_finite() && (0.0..=255.0).contains(&n) && n.fract() == 0.0).then_some(n as u8)
    };
    Some([channel(0)?, channel(1)?, channel(2)?])
}
fn boolean(value: Option<&V>) -> bool {
    matches!(value, Some(V::Bool(true)))
}
fn skip(entry: &Entry<'_>, reason: impl Into<String>) -> V {
    V::Object(vec![
        ("editIndex".into(), V::Number(entry.index as f64)),
        ("pageIndex".into(), V::Number(entry.page as f64)),
        ("reason".into(), V::String(reason.into())),
    ])
}
fn axes(angle: f64) -> [f64; 4] {
    let angle = angle.rem_euclid(360.0);
    if angle == 90.0 {
        [0.0, 1.0, -1.0, 0.0]
    } else if angle == 180.0 {
        [-1.0, 0.0, 0.0, -1.0]
    } else if angle == 270.0 {
        [0.0, -1.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0, 1.0]
    }
}
pub(super) fn save(session: &mut PdfiumSession) -> Result<Vec<u8>, String> {
    let doc = session.document();
    let writer = handle(call(session, "PDFiumExt_OpenFileWriter", &[])?)?
        .ok_or("PDFium could not create a file writer")?;
    let result = (|| {
        if integer(call(
            session,
            "PDFiumExt_SaveAsCopy",
            &[reference(doc), reference(writer)],
        )?)? == 0
        {
            return Err("PDFium could not save the document".into());
        }
        let n = usize::try_from(integer(call(
            session,
            "PDFiumExt_GetFileWriterSize",
            &[reference(writer)],
        )?)?)
        .map_err(|_| "invalid PDF writer size")?;
        buffer(session, n, |s, b| {
            call(
                s,
                "PDFiumExt_GetFileWriterData",
                &[reference(writer), reference(b), json!(n)],
            )?;
            s.read(b, 0, n).map_err(|e| e.to_string())
        })
    })();
    let closed = call(session, "PDFiumExt_CloseFileWriter", &[reference(writer)]);
    match result {
        Err(e) => Err(e),
        Ok(bytes) => {
            closed?;
            Ok(bytes)
        }
    }
}
fn create(
    session: &mut PdfiumSession,
    doc: u64,
    input: &V,
    face: &Face,
) -> Result<Vec<u64>, String> {
    let input_text = text(field(input, "text")).ok_or("invalid inserted text")?;
    let size = number(field(input, "fontSize")).ok_or("invalid inserted font size")?;
    let origin = pair(field(input, "origin")).ok_or("invalid inserted origin")?;
    let color = color(field(input, "color")).ok_or("invalid inserted color")?;
    let leading = number(field(input, "lineLeading")).unwrap_or(size * 1.2);
    let axes = axes(number(field(input, "rotate")).unwrap_or(0.0));
    let offsets = match field(input, "lineXOffsets") {
        Some(V::Array(v)) => v.as_slice(),
        _ => &[],
    };
    let invisible = boolean(field(input, "invisible"));
    let mut created = Vec::new();
    let result = buffer(session, 24, |s, matrix| {
        for (index, line) in input_text.split('\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let obj = handle(call(
                s,
                "FPDFPageObj_CreateTextObj",
                &[reference(doc), reference(face.handle), wire_number(size)],
            )?)?
            .ok_or("PDFium could not create a text object")?;
            created.push(obj);
            let data = line
                .encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>();
            let set = buffer(s, data.len(), |s, b| {
                s.write(b, 0, &data).map_err(|e| e.to_string())?;
                integer(call(
                    s,
                    "FPDFText_SetText",
                    &[reference(obj), reference(b)],
                )?)
            })?;
            if set == 0 {
                return Err("FPDFText_SetText failed on inserted object".into());
            }
            let offset = number(offsets.get(index)).unwrap_or(0.0);
            let drop = index as f64 * leading;
            let values = [
                axes[0],
                axes[1],
                axes[2],
                axes[3],
                origin[0] + axes[0] * offset - axes[2] * drop,
                origin[1] + axes[1] * offset - axes[3] * drop,
            ];
            let bytes = values
                .into_iter()
                .flat_map(|v| (v as f32).to_le_bytes())
                .collect::<Vec<_>>();
            s.write(matrix, 0, &bytes).map_err(|e| e.to_string())?;
            call(
                s,
                "FPDFPageObj_SetMatrix",
                &[reference(obj), reference(matrix)],
            )?;
            call(
                s,
                "FPDFPageObj_SetFillColor",
                &[
                    reference(obj),
                    json!(color[0]),
                    json!(color[1]),
                    json!(color[2]),
                    json!(255),
                ],
            )?;
            if invisible {
                call(
                    s,
                    "FPDFTextObj_SetTextRenderMode",
                    &[reference(obj), json!(3)],
                )?;
            } else if face.bold {
                call(
                    s,
                    "FPDFTextObj_SetTextRenderMode",
                    &[reference(obj), json!(2)],
                )?;
                call(
                    s,
                    "FPDFPageObj_SetStrokeColor",
                    &[
                        reference(obj),
                        json!(color[0]),
                        json!(color[1]),
                        json!(color[2]),
                        json!(255),
                    ],
                )?;
                call(
                    s,
                    "FPDFPageObj_SetStrokeWidth",
                    &[reference(obj), wire_number(size * 0.035)],
                )?;
                call(s, "FPDFPageObj_SetLineJoin", &[reference(obj), json!(1)])?;
            }
        }
        Ok(())
    });
    if let Err(error) = result {
        for obj in created {
            let _ = call(session, "FPDFPageObj_Destroy", &[reference(obj)]);
        }
        return Err(error);
    }
    Ok(created)
}

pub(super) fn apply<F>(
    bytes: &[u8],
    inputs: &[V],
    cancel: &CancelToken,
    relabel: F,
) -> Result<(Vec<u8>, Vec<V>), String>
where
    F: FnOnce(&[u8]) -> Result<Vec<u8>, String>,
{
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    let mut session =
        PdfiumSession::open_with_cancel(bytes, None, cancel.clone()).map_err(|e| e.to_string())?;
    let doc = session.document();
    let count = integer(call(&mut session, "FPDF_GetPageCount", &[reference(doc)])?)?;
    let mut skipped = Vec::new();
    let mut entries = Vec::new();
    for (index, input) in inputs.iter().enumerate() {
        let page = page_index(field(input, "pageIndex"));
        let Some(page) = page.filter(|p| i64::try_from(*p).is_ok_and(|p| p < count)) else {
            skipped.push(V::Object(vec![
                ("editIndex".into(), V::Number(index as f64)),
                (
                    "pageIndex".into(),
                    field(input, "pageIndex").cloned().unwrap_or(V::Undefined),
                ),
                ("reason".into(), V::String("page does not exist".into())),
            ]));
            continue;
        };
        entries.push(Entry { index, page, input })
    }
    let mut groups: Vec<((String, bool, bool), Vec<usize>)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let key = (
            text(field(e.input, "font")).unwrap_or("").to_owned(),
            boolean(field(e.input, "bold")),
            boolean(field(e.input, "italic")),
        );
        if let Some((_, values)) = groups.iter_mut().find(|(k, _)| *k == key) {
            values.push(i)
        } else {
            groups.push((key, vec![i]))
        }
    }
    let mut face_for = HashMap::new();
    let mut faces = Vec::new();
    for ((id, bold, italic), members) in groups {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let merged = members
            .iter()
            .map(|&i| {
                text(field(entries[i].input, "text"))
                    .unwrap_or("")
                    .replace('\n', "")
            })
            .collect::<String>();
        let prepared = font::resolve_insert(
            (!id.is_empty()).then_some(id.as_str()),
            bold,
            italic,
            &merged,
            cancel,
        )
        .and_then(|font| {
            let handle = buffer(&mut session, font.bytes.len(), |s, b| {
                s.write(b, 0, &font.bytes).map_err(|e| e.to_string())?;
                handle(call(
                    s,
                    "FPDFText_LoadFont",
                    &[
                        reference(doc),
                        reference(b),
                        json!(font.bytes.len()),
                        json!(if font.truetype { 2 } else { 1 }),
                        json!(1),
                    ],
                )?)?
                .ok_or_else(|| "FPDFText_LoadFont failed".into())
            })?;
            Ok(Face {
                handle,
                truetype: font.truetype,
                bold: font.synthetic_bold,
            })
        });
        match prepared {
            Ok(face) => {
                let i = faces.len();
                faces.push(face);
                for entry in members {
                    face_for.insert(entry, i);
                }
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    return Err("cancelled".into());
                }
                for entry in members {
                    skipped.push(skip(&entries[entry], &e));
                }
            }
        }
    }
    let mut pages: Vec<(usize, Vec<usize>)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if let Some((_, group)) = pages.iter_mut().find(|(p, _)| *p == e.page) {
            group.push(i)
        } else {
            pages.push((e.page, vec![i]))
        }
    }
    let mut total = 0;
    let mut cff = false;
    for (index, group) in pages {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let page = handle(call(
            &mut session,
            "FPDF_LoadPage",
            &[reference(doc), json!(index)],
        )?)?
        .ok_or_else(|| format!("could not load page {}", index + 1))?;
        let result = (|| {
            let mut applied = 0;
            for entry in group {
                let e = &entries[entry];
                if text(field(e.input, "text")).unwrap_or("").trim().is_empty() {
                    skipped.push(skip(e, "empty inserted text"));
                    continue;
                }
                let Some(face) = face_for.get(&entry).map(|i| &faces[*i]) else {
                    continue;
                };
                match create(&mut session, doc, e.input, face) {
                    Err(error) => {
                        if cancel.is_cancelled() {
                            return Err("cancelled".into());
                        }
                        skipped.push(skip(e, error));
                    }
                    Ok(objects) => {
                        for obj in objects {
                            call(
                                &mut session,
                                "FPDFPage_InsertObject",
                                &[reference(page), reference(obj)],
                            )?;
                        }
                        cff |= !face.truetype;
                        applied += 1
                    }
                }
            }
            if applied > 0
                && integer(call(
                    &mut session,
                    "FPDFPage_GenerateContent",
                    &[reference(page)],
                )?)? == 0
            {
                return Err(format!("could not regenerate page {}", index + 1));
            }
            total += applied;
            Ok(())
        })();
        let closed = call(&mut session, "FPDF_ClosePage", &[reference(page)]);
        result?;
        closed?;
    }
    if total == 0 {
        return Ok((bytes.to_vec(), skipped));
    }
    let saved = save(&mut session)?;
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    Ok((if cff { relabel(&saved)? } else { saved }, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn insertion_rotation_uses_exact_quarter_turns() {
        assert_eq!(axes(90.0), [0.0, 1.0, -1.0, 0.0]);
        assert_eq!(axes(-90.0), [0.0, -1.0, 1.0, 0.0]);
        assert_eq!(axes(90.5), [1.0, 0.0, 0.0, 1.0]);
    }
}
