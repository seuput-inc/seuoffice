//! Delete annotations only when their stable identity still matches.

use super::super::pdfium_session::PdfiumSession;
use serde_json::{Value, json};
use shell_contract::{IpcValue, runtime::CancelToken};

fn field<'a>(value: &'a IpcValue, name: &str) -> Option<&'a IpcValue> {
    match value {
        IpcValue::Object(entries) => entries
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value),
        _ => None,
    }
}
fn number(value: Option<&IpcValue>) -> Option<f64> {
    match value {
        Some(IpcValue::Number(value)) => Some(*value),
        _ => None,
    }
}
fn text(value: Option<&IpcValue>) -> Option<&str> {
    match value {
        Some(IpcValue::String(value)) => Some(value),
        _ => None,
    }
}
fn rect(value: Option<&IpcValue>) -> Option<[f64; 4]> {
    let Some(IpcValue::Array(values)) = value else {
        return None;
    };
    if values.len() != 4 {
        return None;
    }
    Some([
        number(values.first())?,
        number(values.get(1))?,
        number(values.get(2))?,
        number(values.get(3))?,
    ])
}
fn handle(value: &Value) -> Option<u64> {
    value
        .get("ref")
        .and_then(Value::as_u64)
        .filter(|id| *id != 0)
}
fn truthy(value: &Value) -> bool {
    value.as_i64().is_some_and(|value| value != 0)
}
fn reference(id: u64) -> Value {
    json!({"ref": id})
}
fn call(session: &mut PdfiumSession, name: &str, args: &[Value]) -> Result<Value, String> {
    session.call(name, args).map_err(|error| error.to_string())
}

fn rect_matches(actual: [f64; 4], expected: [f64; 4], note: bool) -> bool {
    let norm = |r: [f64; 4]| {
        [
            r[0].min(r[2]),
            r[1].min(r[3]),
            r[0].max(r[2]),
            r[1].max(r[3]),
        ]
    };
    let (a, b) = (norm(actual), norm(expected));
    if note {
        (a[0] - b[0]).abs() <= 2.0 && (a[3] - b[3]).abs() <= 2.0
    } else {
        a.iter().zip(b).all(|(a, b)| (a - b).abs() <= 2.0)
    }
}

fn matches(session: &mut PdfiumSession, annot: u64, delete: &IpcValue) -> Result<bool, String> {
    let subtype = match text(field(delete, "subtype")) {
        Some("highlight") => 9,
        Some("underline") => 10,
        Some("squiggly") => 11,
        Some("strikeout") => 12,
        Some("note") => 1,
        Some("polygon") => 7,
        _ => return Ok(false),
    };
    if call(session, "FPDFAnnot_GetSubtype", &[reference(annot)])?.as_i64() != Some(subtype) {
        return Ok(false);
    }
    let Some(expected) = rect(field(delete, "rect")) else {
        return Ok(false);
    };
    let buffer = session.alloc(16).map_err(|error| error.to_string())?;
    let got = call(
        session,
        "FPDFAnnot_GetRect",
        &[reference(annot), reference(buffer)],
    )?;
    let raw = if truthy(&got) {
        Some(
            session
                .read(buffer, 0, 16)
                .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };
    session.free(buffer).map_err(|error| error.to_string())?;
    let Some(raw) = raw else { return Ok(false) };
    let mut actual = [0.0; 4];
    for (index, value) in actual.iter_mut().enumerate() {
        let at = index * 4;
        *value = f32::from_le_bytes(
            raw[at..at + 4]
                .try_into()
                .map_err(|_| "invalid annotation rectangle")?,
        ) as f64;
    }
    if !rect_matches(actual, expected, subtype == 1) {
        return Ok(false);
    }
    let expected = match field(delete, "contents") {
        None | Some(IpcValue::Undefined) => return Ok(true),
        Some(IpcValue::String(value)) => value.as_str(),
        _ => return Ok(false),
    };
    let key = session.alloc(9).map_err(|error| error.to_string())?;
    session
        .write(key, 0, b"Contents\0")
        .map_err(|error| error.to_string())?;
    let len = call(
        session,
        "FPDFAnnot_GetStringValue",
        &[reference(annot), reference(key), json!(0), json!(0)],
    )?
    .as_u64()
    .ok_or("invalid annotation contents size")? as usize;
    let content = if len <= 2 {
        String::new()
    } else {
        let buffer = session.alloc(len).map_err(|error| error.to_string())?;
        call(
            session,
            "FPDFAnnot_GetStringValue",
            &[
                reference(annot),
                reference(key),
                reference(buffer),
                json!(len),
            ],
        )?;
        let bytes = session
            .read(buffer, 0, len - 2)
            .map_err(|error| error.to_string())?;
        session.free(buffer).map_err(|error| error.to_string())?;
        String::from_utf16_lossy(
            &bytes
                .chunks_exact(2)
                .map(|part| u16::from_le_bytes([part[0], part[1]]))
                .collect::<Vec<_>>(),
        )
    };
    session.free(key).map_err(|error| error.to_string())?;
    Ok(content == expected)
}

pub(super) fn remove(
    session: &mut PdfiumSession,
    page: u64,
    delete: &IpcValue,
) -> Result<bool, String> {
    let object = number(field(delete, "objNum")).unwrap_or(0.0);
    let by_object = call(
        session,
        "EPDFPage_GetAnnotByObjectNumber",
        &[reference(page), json!(object)],
    )?;
    if let Some(annot) = handle(&by_object) {
        let matched = matches(session, annot, delete)?;
        call(session, "FPDFPage_CloseAnnot", &[reference(annot)])?;
        if matched
            && truthy(&call(
                session,
                "EPDFPage_RemoveAnnotByObjectNumber",
                &[reference(page), json!(object)],
            )?)
        {
            return Ok(true);
        }
    }
    let count = call(session, "FPDFPage_GetAnnotCount", &[reference(page)])?
        .as_i64()
        .ok_or("invalid annotation count")?;
    for index in 0..count {
        let value = call(
            session,
            "FPDFPage_GetAnnot",
            &[reference(page), json!(index)],
        )?;
        let Some(annot) = handle(&value) else {
            continue;
        };
        let matched = matches(session, annot, delete)?;
        call(session, "FPDFPage_CloseAnnot", &[reference(annot)])?;
        if matched {
            return Ok(truthy(&call(
                session,
                "FPDFPage_RemoveAnnot",
                &[reference(page), json!(index)],
            )?));
        }
    }
    Ok(false)
}

pub(super) fn apply(
    bytes: &[u8],
    deletes: &[IpcValue],
    cancel: &CancelToken,
) -> Result<Vec<u8>, String> {
    if cancel.is_cancelled() {
        return Err("PDF save cancelled".into());
    }
    if deletes.is_empty() {
        return Ok(bytes.to_vec());
    }
    let mut session = PdfiumSession::open_with_cancel(bytes, None, cancel.clone())
        .map_err(|error| error.to_string())?;
    let doc = session.document();
    let count = call(&mut session, "FPDF_GetPageCount", &[reference(doc)])?
        .as_i64()
        .ok_or("invalid PDF page count")?;
    let mut pages: Vec<(i64, Vec<&IpcValue>)> = Vec::new();
    for delete in deletes {
        let Some(page) = number(field(delete, "pageIndex")) else {
            continue;
        };
        if page < 0.0 || page >= count as f64 {
            continue;
        }
        let page = page as i64;
        if let Some((_, group)) = pages.iter_mut().find(|(index, _)| *index == page) {
            group.push(delete);
        } else {
            pages.push((page, vec![delete]));
        }
    }
    let mut removed = 0;
    for (index, group) in pages {
        let page = call(
            &mut session,
            "FPDF_LoadPage",
            &[reference(doc), json!(index)],
        )?;
        let Some(page) = handle(&page) else { continue };
        for delete in group {
            if remove(&mut session, page, delete)? {
                removed += 1;
            }
        }
        call(&mut session, "FPDF_ClosePage", &[reference(page)])?;
    }
    if removed == 0 {
        return Ok(bytes.to_vec());
    }
    let writer = handle(&call(&mut session, "PDFiumExt_OpenFileWriter", &[])?)
        .ok_or("PDFium could not open writer")?;
    if !truthy(&call(
        &mut session,
        "PDFiumExt_SaveAsCopy",
        &[reference(doc), reference(writer)],
    )?) {
        return Err("PDFium could not save PDF".into());
    }
    let size = call(
        &mut session,
        "PDFiumExt_GetFileWriterSize",
        &[reference(writer)],
    )?
    .as_u64()
    .and_then(|value| usize::try_from(value).ok())
    .ok_or("invalid PDFium writer size")?;
    let buffer = session.alloc(size).map_err(|error| error.to_string())?;
    call(
        &mut session,
        "PDFiumExt_GetFileWriterData",
        &[reference(writer), reference(buffer), json!(size)],
    )?;
    let output = session
        .read(buffer, 0, size)
        .map_err(|error| error.to_string())?;
    session.free(buffer).map_err(|error| error.to_string())?;
    call(
        &mut session,
        "PDFiumExt_CloseFileWriter",
        &[reference(writer)],
    )?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_match_top_left_while_other_annotations_match_all_bounds() {
        let original = [10.0, 20.0, 30.0, 40.0];
        let resized = [11.0, 5.0, 50.0, 39.0];
        assert!(rect_matches(original, resized, true));
        assert!(!rect_matches(original, resized, false));
        assert!(!rect_matches(original, [13.0, 20.0, 30.0, 40.0], true));
    }
}
