//! Page and metadata decisions for a single document-edit session.
use super::session::Session;
use serde_json::{Value, json};
use shell_contract::IpcValue;

fn field<'a>(v: &'a IpcValue, k: &str) -> Option<&'a IpcValue> {
    match v {
        IpcValue::Object(o) => o.iter().find(|(n, _)| n == k).map(|(_, v)| v),
        _ => None,
    }
}
fn list(v: Option<&IpcValue>) -> &[IpcValue] {
    match v {
        Some(IpcValue::Array(a)) => a,
        _ => &[],
    }
}
fn number(v: Option<&IpcValue>) -> Option<f64> {
    match v {
        Some(IpcValue::Number(n)) => Some(*n),
        _ => None,
    }
}
fn index(v: Option<&IpcValue>, count: usize) -> Option<usize> {
    number(v)
        .filter(|v| v.is_finite() && *v >= 0.0 && v.fract() == 0.0 && *v < count as f64)
        .map(|v| v as usize)
}
fn json_value(v: &IpcValue) -> Value {
    match v {
        IpcValue::Undefined | IpcValue::Null => Value::Null,
        IpcValue::Bool(b) => json!(b),
        IpcValue::Number(n) | IpcValue::Date(n) => json!(n),
        IpcValue::String(s) => json!(s),
        IpcValue::Array(a) => Value::Array(a.iter().map(json_value).collect()),
        IpcValue::Object(o) => Value::Object(
            o.iter()
                .filter(|(_, v)| !matches!(v, IpcValue::Undefined))
                .map(|(k, v)| (k.clone(), json_value(v)))
                .collect(),
        ),
        IpcValue::Bytes(_, _) => Value::Null,
    }
}
pub(super) fn rotations(
    session: &mut Session,
    request: &IpcValue,
    page_count: usize,
) -> Result<(), String> {
    for r in list(field(request, "rotations")) {
        let Some(page) = index(field(r, "pageIndex"), page_count) else {
            continue;
        };
        let delta = number(field(r, "delta")).unwrap_or(f64::NAN);
        let angle = session
            .exchange(json!({"op":"rotation","index":page}))?
            .as_f64()
            .ok_or("invalid PDF page rotation")?;
        session.exchange(json!({"op":"rotatePage","index":page,"angle":(angle+delta)%360.0}))?;
    }
    Ok(())
}
pub(super) fn metadata(
    session: &mut Session,
    request: &IpcValue,
    modified_at: f64,
) -> Result<(), String> {
    let Some(meta) =
        field(request, "metadata").filter(|v| !matches!(v, IpcValue::Undefined | IpcValue::Null))
    else {
        return Ok(());
    };
    for key in ["title", "author", "subject"] {
        if let Some(IpcValue::String(value)) = field(meta, key) {
            session.exchange(json!({"op":"metadataString","key":key,"value":value}))?;
        }
    }
    if let Some(IpcValue::String(value)) = field(meta, "keywords") {
        let words: Vec<_> = value
            .split([',', '，', ';', '；'])
            .map(jscompat::trim)
            .filter(|s| !s.is_empty())
            .collect();
        session.exchange(json!({"op":"metadataKeywords","value":words}))?;
    }
    session.exchange(json!({"op":"metadataDate","value":modified_at}))?;
    Ok(())
}
pub(super) fn finish_pages(
    session: &mut Session,
    request: &IpcValue,
    page_count: usize,
) -> Result<(), String> {
    for redaction in list(field(request, "redactions")) {
        if let Some(page) = index(field(redaction, "pageIndex"), page_count) {
            let node = session
                .exchange(json!({"op":"page","index":page}))?
                .as_u64()
                .ok_or("invalid PDF page handle")?;
            for key in ["Thumb", "PieceInfo"] {
                session.exchange(json!({"op":"delete","target":node,"key":key}))?;
            }
        }
    }
    let mut deleted: Vec<_> = list(field(request, "deletedPages"))
        .iter()
        .filter_map(|v| number(Some(v)))
        .collect();
    deleted.sort_by(|a, b| b.total_cmp(a));
    let mut remaining = page_count;
    for value in &deleted {
        if value.is_finite()
            && *value >= 0.0
            && value.fract() == 0.0
            && *value < remaining as f64
            && remaining > 1
        {
            session.exchange(json!({"op":"removePage","index":*value as usize}))?;
            remaining -= 1;
        }
    }
    if let Some(IpcValue::Array(order)) = field(request, "pageOrder")
        && !order.is_empty()
    {
        let target: Vec<_> = order
            .iter()
            .filter_map(|v| number(Some(v)))
            .filter(|v| !deleted.contains(v))
            .filter(|v| v.is_finite() && *v >= 0.0 && v.fract() == 0.0 && *v < page_count as f64)
            .map(|v| v as usize)
            .collect();
        if target.len() == remaining {
            session.exchange(json!({"op":"reorderPages","indices":target}))?;
        }
    }
    if let Some(fills) = super::super::static_fills::resulting_fills(request, page_count) {
        let catalog = session
            .exchange(json!({"op":"catalog"}))?
            .as_u64()
            .ok_or("invalid PDF catalog handle")?;
        if matches!(&fills,IpcValue::Array(a) if a.is_empty()) {
            session.exchange(
                json!({"op":"delete","target":catalog,"key":"GenOfficeStaticFormFills"}),
            )?;
        } else {
            let text = serde_json::to_string(&json_value(&fills)).map_err(|e| e.to_string())?;
            session.exchange(json!({"op":"set","target":catalog,"key":"GenOfficeStaticFormFills","value":{"text":text}}))?;
        }
    }
    Ok(())
}
pub(super) fn appearances(session: &mut Session) -> Result<(), String> {
    let fields = session.exchange(json!({"op":"formDirtyFields"}))?;
    let fields = fields.as_array().ok_or("invalid PDF form fields")?;
    let mut failed = false;
    for index in fields {
        if session
            .exchange(json!({"op":"updateFieldAppearance","index":index}))
            .is_err()
        {
            failed = true;
        }
    }
    if failed {
        session.exchange(json!({"op":"setNeedAppearances"}))?;
    }
    Ok(())
}
