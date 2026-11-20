//! Place rasterized watermark and header/footer stamps over page contents.
use super::session::Session;
use serde_json::json;
use shell_contract::IpcValue;
fn field<'a>(v: &'a IpcValue, key: &str) -> Option<&'a IpcValue> {
    match v {
        IpcValue::Object(o) => o.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}
fn number(v: Option<&IpcValue>) -> Option<f64> {
    match v {
        Some(IpcValue::Number(n)) => Some(*n),
        _ => None,
    }
}
pub(super) fn apply(
    session: &mut Session,
    stamps: &[IpcValue],
    page_count: usize,
) -> Result<(), String> {
    for stamp in stamps {
        let Some(page) = number(field(stamp, "pageIndex"))
            .filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0 && *n < page_count as f64)
        else {
            continue;
        };
        let Some(IpcValue::String(image)) = field(stamp, "image") else {
            return Err("invalid stamp image".into());
        };
        let Some(IpcValue::Array(rect)) = field(stamp, "rect") else {
            return Err("invalid stamp rectangle".into());
        };
        if rect.len() != 4 {
            return Err("invalid stamp rectangle".into());
        }
        let mut r = [0.0; 4];
        for (i, v) in rect.iter().enumerate() {
            r[i] = number(Some(v)).ok_or("invalid stamp rectangle")?;
        }
        session.exchange(json!({"op":"drawPng","index":page as usize,"image":image,"x":r[0],"y":r[1],"width":r[2]-r[0],"height":r[3]-r[1],"opacity":number(field(stamp,"opacity")).unwrap_or(1.0)}))?;
    }
    Ok(())
}
