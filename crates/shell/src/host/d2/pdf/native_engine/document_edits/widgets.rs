//! AcroForm widget policy over the request-owned pdf-lib object session.
use serde_json::{Value, json};
use shell_contract::IpcValue;
use super::{field, list};
use super::forms::{date_format, widget_box};
use super::session::Session;

fn text<'a>(value: Option<&'a IpcValue>) -> Option<&'a str> {
    match value { Some(IpcValue::String(value)) => Some(value), _ => None }
}
fn number(value: Option<&IpcValue>) -> Option<f64> {
    match value { Some(IpcValue::Number(value)) => Some(*value), _ => None }
}
fn boolean(value: Option<&IpcValue>) -> Option<bool> {
    match value { Some(IpcValue::Bool(value)) => Some(*value), _ => None }
}
fn index(value: Option<&IpcValue>, count: usize) -> Option<usize> {
    let value = number(value)?;
    (value.is_finite() && value >= 0. && value.fract() == 0. && value < count as f64).then_some(value as usize)
}
fn rect(value: Option<&IpcValue>) -> Result<[f64; 4], String> {
    let Some(IpcValue::Array(values)) = value else { return Err("invalid form widget rectangle".into()) };
    if values.len() != 4 { return Err("invalid form widget rectangle".into()); }
    let mut rect = [0.; 4];
    for (slot, value) in rect.iter_mut().zip(values) {
        *slot = number(Some(value)).ok_or("invalid form widget rectangle")?;
    }
    Ok(rect)
}
fn normalized(rect: [f64; 4]) -> [f64; 4] {
    [jscompat::math_min(rect[0], rect[2]), jscompat::math_min(rect[1], rect[3]), jscompat::math_max(rect[0], rect[2]), jscompat::math_max(rect[1], rect[3])]
        .map(|n| jscompat::math_round(n * 100.) / 100.)
}
fn handle(value: &Value) -> Result<u64, String> { value.as_u64().filter(|n| *n != 0).ok_or_else(|| "invalid PDF object handle".into()) }
fn command(session: &mut Session, value: Value) -> Result<(), String> { session.exchange(value).map(|_| ()) }
fn get_handle(session: &mut Session, value: Value) -> Result<u64, String> { handle(&session.exchange(value)?) }
fn same(session: &mut Session, left: u64, right: u64) -> Result<bool, String> {
    session.exchange(json!({"op":"same", "left":left, "right":right}))?.as_bool().ok_or_else(|| "invalid PDF identity response".into())
}
fn widget_ref(id: &str) -> Option<(u64, u64)> {
    let (object, generation) = id.split_once('R')?;
    if object.is_empty() || !object.bytes().all(|b| b.is_ascii_digit()) || !generation.bytes().all(|b| b.is_ascii_digit()) { return None; }
    Some((object.parse().ok()?, if generation.is_empty() { 0 } else { generation.parse().ok()? }))
}
fn append_annotation(session: &mut Session, page: usize, reference: u64) -> Result<(), String> {
    let page = get_handle(session, json!({"op":"page", "index":page}))?;
    let current = session.exchange(json!({"op":"read", "target":page, "key":"Annots", "resolve":true}))?;
    let array = if current.is_null() {
        let array = get_handle(session, json!({"op":"make", "value":{"array":[]}}))?;
        command(session, json!({"op":"set", "target":page, "key":"Annots", "value":{"ref":array}}))?;
        array
    } else { handle(&current)? };
    command(session, json!({"op":"push", "target":array, "value":{"ref":reference}}))
}
fn detach_annotation(session: &mut Session, page_count: usize, reference: u64) -> Result<(), String> {
    for index in 0..page_count {
        let page = get_handle(session, json!({"op":"page", "index":index}))?;
        let array = session.exchange(json!({"op":"read", "target":page, "key":"Annots", "resolve":true}))?;
        if array.is_null() { continue; }
        let array = handle(&array)?;
        let size = session.exchange(json!({"op":"arraySize", "target":array}))?.as_u64().ok_or("invalid PDF annotation array size")?;
        for at in (0..size).rev() {
            let item = get_handle(session, json!({"op":"arrayRead", "target":array, "index":at}))?;
            if same(session, item, reference)? { command(session, json!({"op":"remove", "target":array, "index":at}))?; }
        }
    }
    Ok(())
}
fn install_date_actions(session: &mut Session, target: u64, format: &str) -> Result<(), String> {
    let format = date_format(format);
    let actions = json!({"dict":{
        "F":{"dict":{"S":{"name":"JavaScript"},"JS":{"literal":format!("AFDate_FormatEx(\"{format}\");")}}},
        "K":{"dict":{"S":{"name":"JavaScript"},"JS":{"literal":format!("AFDate_KeystrokeEx(\"{format}\");")}}}
    }});
    command(session, json!({"op":"set", "target":target, "key":"AA", "value":actions}))
}
fn add_signature(session: &mut Session, page: usize, name: &str, required: bool, area: [f64; 4]) -> Result<(), String> {
    let page_ref = get_handle(session, json!({"op":"pageRef", "index":page}))?;
    let area = normalized(area);
    let value = json!({"dict":{
        "Type":{"name":"Annot"}, "Subtype":{"name":"Widget"}, "FT":{"name":"Sig"},
        "T":{"text":name}, "Ff":if required {2} else {0}, "F":4,
        "Rect":{"array":area}, "P":{"ref":page_ref}, "MK":{"dict":{"BC":{"array":[0.45,0.45,0.45]}}}
    }});
    let reference = get_handle(session, json!({"op":"make", "value":value, "register":true}))?;
    command(session, json!({"op":"formAddField", "ref":reference}))?;
    append_annotation(session, page, reference)
}
fn apply_widget_edits(session: &mut Session, edits: &[IpcValue], page_count: usize) -> Result<(), String> {
    for edit in edits {
        let name = text(field(edit, "fieldName")).unwrap_or("");
        let found = session.exchange(json!({"op":"fieldFind", "name":name}))?;
        if found.is_null() { continue; }
        let field_handle = handle(found.get("handle").ok_or("invalid PDF field response")?)?;
        let kind = found.get("kind").and_then(Value::as_str).ok_or("invalid PDF field response")?;
        let widgets = found.get("widgets").and_then(Value::as_array).ok_or("invalid PDF field widgets")?;
        let candidate = if let Some((object, generation)) = text(field(edit, "widgetId")).and_then(widget_ref) {
            let value = session.exchange(json!({"op":"lookupRef", "object":object, "generation":generation}))?;
            if value.is_null() { None } else { Some(handle(&value)?) }
        } else { None };
        let mut selected = None;
        if let Some(candidate) = candidate {
            for (index, widget) in widgets.iter().enumerate() {
                if same(session, handle(widget.get("dict").ok_or("invalid PDF widget")?)?, candidate)? { selected = Some(index); break; }
            }
        }
        let Some(index) = selected.or_else(|| (widgets.len() == 1).then_some(0)) else { continue };
        let widget = &widgets[index];
        let dict = handle(widget.get("dict").ok_or("invalid PDF widget")?)?;
        let reference = handle(widget.get("ref").ok_or("invalid PDF widget")?)?;
        if boolean(field(edit, "remove")) == Some(true) {
            if widgets.len() <= 1 { command(session, json!({"op":"fieldRemove", "field":field_handle}))?; }
            else {
                command(session, json!({"op":"fieldRemoveWidget", "field":field_handle, "index":index}))?;
                detach_annotation(session, page_count, reference)?;
            }
            continue;
        }
        if let Some(area) = field(edit, "rect").filter(|v| !matches!(v, IpcValue::Null | IpcValue::Undefined)) {
            command(session, json!({"op":"set", "target":dict, "key":"Rect", "value":{"array":normalized(rect(Some(area))?)}}))?;
            if kind != "signature" { command(session, json!({"op":"fieldDirty", "field":field_handle}))?; }
        }
        if let Some(required) = boolean(field(edit, "required")) {
            command(session, json!({"op":"fieldRequired", "field":field_handle, "value":required}))?;
        }
    }
    Ok(())
}
fn apply_fields(session: &mut Session, fields: &[IpcValue], page_count: usize) -> Result<(), String> {
    for entry in fields {
        let Some(page) = index(field(entry, "pageIndex"), page_count) else { continue };
        let name = text(field(entry, "name")).unwrap_or("");
        let kind = text(field(entry, "kind")).unwrap_or("");
        let existing = if name.is_empty() { Value::Null } else { session.exchange(json!({"op":"fieldFind", "name":name}))? };
        if name.is_empty() || (!existing.is_null() && !(kind == "radio" && existing.get("kind").and_then(Value::as_str) == Some("radio"))) {
            return Err(format!("A form field named \"{name}\" already exists"));
        }
        if existing.is_null() {
            let names = session.exchange(json!({"op":"fieldNames"}))?;
            let names = names.as_array().ok_or("invalid PDF field names")?;
            let prefix = format!("{name}.");
            if names.iter().any(|value| value.as_str().is_some_and(|s| s.starts_with(&prefix))) {
                return Err(format!("A form field group named \"{name}\" already exists"));
            }
        }
        let area = rect(field(entry, "rect"))?;
        let required = boolean(field(entry, "required")) == Some(true);
        if kind == "signature" { add_signature(session, page, name, required, area)?; continue; }
        let kind = match kind { "checkbox" | "radio" | "choice" => kind, _ => "text" };
        let rotation = session.exchange(json!({"op":"rotation", "index":page}))?.as_f64().ok_or("invalid PDF page rotation")?;
        let area = widget_box(area, rotation);
        let field_handle = if existing.is_null() {
            get_handle(session, json!({"op":"fieldCreate", "name":name, "kind":kind}))?
        } else { handle(existing.get("handle").ok_or("invalid PDF field response")?)? };
        if required { command(session, json!({"op":"fieldRequired", "field":field_handle, "value":true}))?; }
        let mut option = None;
        if kind == "radio" {
            let options = session.exchange(json!({"op":"fieldOptions", "field":field_handle}))?;
            let count = options.as_array().ok_or("invalid PDF radio options")?.len();
            option = Some(text(field(entry, "exportValue")).filter(|value| !value.is_empty()).map(str::to_owned).unwrap_or_else(|| format!("option_{}", count + 1)));
        } else if kind == "choice" {
            let mut options = Vec::new();
            for value in list(field(entry, "options")) {
                let value = text(Some(value)).ok_or("invalid PDF choice option")?;
                let value = jscompat::trim(value);
                if !value.is_empty() { options.push(value.to_owned()); }
            }
            command(session, json!({"op":"fieldAddOptions", "field":field_handle, "options":options}))?;
            if let Some(value) = text(field(entry, "value")).filter(|value| !value.is_empty() && options.iter().any(|option| option == *value)) {
                command(session, json!({"op":"fieldSelect", "field":field_handle, "value":value}))?;
            }
        } else if kind == "text" {
            if boolean(field(entry, "multiLine")) == Some(true) { command(session, json!({"op":"fieldMultiline", "field":field_handle}))?; }
            if let Some(value) = number(field(entry, "maxLen")).filter(|n| *n > 0.) { command(session, json!({"op":"fieldMaxLength", "field":field_handle, "value":value.floor()}))?; }
            let alignment = text(field(entry, "textAlignment")).unwrap_or("left");
            command(session, json!({"op":"fieldAlignment", "field":field_handle, "value":alignment}))?;
        }
        command(session, json!({"op":"fieldAddToPage", "field":field_handle, "page":page,
            "box":{"x":area.x,"y":area.y,"width":area.width,"height":area.height,"rotation":rotation}, "option":option}))?;
        if kind == "checkbox" && boolean(field(entry, "checked")) == Some(true) {
            command(session, json!({"op":"fieldCheck", "field":field_handle, "checked":true}))?;
        } else if kind == "radio" && boolean(field(entry, "checked")) == Some(true) {
            command(session, json!({"op":"fieldSelect", "field":field_handle, "value":option}))?;
        } else if kind == "text" {
            if let Some(value) = text(field(entry, "value")).filter(|value| !value.is_empty()) { command(session, json!({"op":"fieldText", "field":field_handle, "value":value}))?; }
            if let Some(value) = number(field(entry, "fontSize")).filter(|n| *n > 0.) { command(session, json!({"op":"fieldFontSize", "field":field_handle, "value":value}))?; }
            if let Some(format) = text(field(entry, "dateFormat")).filter(|format| !format.is_empty()) {
                let field = session.exchange(json!({"op":"fieldFind", "name":name}))?;
                let reference = handle(field.get("ref").ok_or("invalid PDF field reference")?)?;
                let dict = get_handle(session, json!({"op":"resolve", "target":reference}))?;
                install_date_actions(session, dict, format)?;
                let widgets = field.get("widgets").and_then(Value::as_array).ok_or("invalid PDF field widgets")?;
                if let Some(widget) = widgets.last() { install_date_actions(session, handle(widget.get("dict").ok_or("invalid PDF widget")?)?, format)?; }
            }
        }
    }
    Ok(())
}
/// Run edits before new fields, matching the original save ordering.
pub(super) fn apply(session: &mut Session, request: &IpcValue, page_count: usize) -> Result<(), String> {
    apply_widget_edits(session, list(field(request, "formWidgetEdits")), page_count)?;
    apply_fields(session, list(field(request, "formFields")), page_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn widget_ids_and_normalized_rectangles_follow_pdf_input() {
        assert_eq!(widget_ref("12R"), Some((12, 0)));
        assert_eq!(widget_ref("12R3"), Some((12, 3)));
        assert_eq!(widget_ref("12R3x"), None);
        assert_eq!(normalized([12.345, 8., 2., 3.]), [2., 3., 12.35, 8.]);
    }
}
