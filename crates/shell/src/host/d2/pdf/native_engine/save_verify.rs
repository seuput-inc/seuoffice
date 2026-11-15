//! Map applied content edits to their final pages for save read-back verification.

use shell_contract::IpcValue;
use shell_ipc_d2::pdf::editor::hosts::SaveSkips;

fn field<'a>(value: &'a IpcValue, name: &str) -> Option<&'a IpcValue> {
    match value { IpcValue::Object(entries) => entries.iter().find(|(key, _)| key == name).map(|(_, value)| value), _ => None }
}
fn array(value: Option<&IpcValue>) -> &[IpcValue] {
    match value { Some(IpcValue::Array(values)) => values, _ => &[] }
}
fn number(value: Option<&IpcValue>) -> Option<f64> {
    match value { Some(IpcValue::Number(value)) => Some(*value), _ => None }
}
fn text(value: Option<&IpcValue>) -> Option<&str> {
    match value { Some(IpcValue::String(value)) => Some(value), _ => None }
}

fn final_page(request: &IpcValue, page: f64) -> Option<f64> {
    if let Some(IpcValue::Array(order)) = field(request, "pageOrder") {
        return order.iter().position(|value| number(Some(value)) == Some(page)).map(|index| index as f64);
    }
    let deleted = array(field(request, "deletedPages"));
    if deleted.iter().any(|value| number(Some(value)) == Some(page)) { return None; }
    Some(page - deleted.iter().filter(|value| number(Some(value)).is_some_and(|deleted| deleted < page)).count() as f64)
}

pub(super) struct Verification {
    pub texts: Vec<IpcValue>,
    pub images: Vec<IpcValue>,
    pub password: Option<String>,
}

pub(super) fn verification(request: &IpcValue, skips: &SaveSkips) -> Verification {
    let mut texts = Vec::new();
    for edit in array(field(request, "textEdits")) {
        let Some(page) = number(field(edit, "pageIndex")) else { continue };
        if skips.text_edits.iter().any(|skip| number(field(skip, "pageIndex")) == Some(page) && text(field(skip, "oldText")) == text(field(edit, "oldText"))) { continue; }
        if let (Some(page), Some(value)) = (final_page(request, page), field(edit, "newText")) {
            texts.push(IpcValue::Object(vec![("pageIndex".into(), IpcValue::Number(page)), ("newText".into(), value.clone())]));
        }
    }
    for (index, edit) in array(field(request, "textInserts")).iter().enumerate() {
        if skips.text_inserts.iter().any(|skip| number(field(skip, "editIndex")) == Some(index as f64)) { continue; }
        if let (Some(page), Some(value)) = (number(field(edit, "pageIndex")).and_then(|page| final_page(request, page)), field(edit, "text")) {
            texts.push(IpcValue::Object(vec![("pageIndex".into(), IpcValue::Number(page)), ("newText".into(), value.clone())]));
        }
    }
    let mut images = Vec::new();
    for (index, edit) in array(field(request, "imageEdits")).iter().enumerate() {
        if text(field(edit, "kind")) == Some("deleteImage") || skips.image_edits.iter().any(|skip| number(field(skip, "editIndex")) == Some(index as f64)) { continue; }
        if let (Some(page), Some(rect)) = (number(field(edit, "pageIndex")).and_then(|page| final_page(request, page)), field(edit, "rect")) {
            images.push(IpcValue::Object(vec![("pageIndex".into(), IpcValue::Number(page)), ("rect".into(), rect.clone())]));
        }
    }
    let protection = if matches!(field(request, "protectionDirty"), Some(IpcValue::Bool(false))) { "decryption" } else { "encryption" };
    let password = field(request, protection).and_then(|value| text(field(value, "userPassword"))).map(str::to_owned);
    Verification { texts, images, password }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn object(entries: Vec<(&str, IpcValue)>) -> IpcValue { IpcValue::Object(entries.into_iter().map(|(key, value)| (key.into(), value)).collect()) }
    #[test]
    fn removed_and_skipped_edits_are_not_verified() {
        let edit = |page, value: &str| object(vec![("pageIndex", IpcValue::Number(page)), ("oldText", IpcValue::String(value.into())), ("newText", IpcValue::String(value.into()))]);
        let request = object(vec![("deletedPages", IpcValue::Array(vec![IpcValue::Number(0.0)])), ("textEdits", IpcValue::Array(vec![edit(0.0, "removed"), edit(2.0, "kept")]))]);
        let result = verification(&request, &SaveSkips::default());
        assert_eq!(result.texts, vec![object(vec![("pageIndex", IpcValue::Number(1.0)), ("newText", IpcValue::String("kept".into()))])]);
    }
}
