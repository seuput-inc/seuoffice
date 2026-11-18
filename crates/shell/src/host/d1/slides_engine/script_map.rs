use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::EngineError;
use shell_ipc_d1::value::{field, truthy};

fn number(value: &IpcValue) -> f64 {
    match value {
        IpcValue::Number(value) => *value,
        IpcValue::Null => 0.0,
        IpcValue::Undefined => f64::NAN,
        IpcValue::Bool(value) => if *value { 1.0 } else { 0.0 },
        other => jscompat::to_number(&shell_ipc_d1::slides::session::js_string(other)),
    }
}

fn object(fields: impl IntoIterator<Item = (&'static str, IpcValue)>) -> IpcValue {
    IpcValue::Object(fields.into_iter().map(|(key, value)| (key.to_owned(), value)).collect())
}

pub(super) fn map(req: &IpcValue, width: f64) -> Result<Vec<IpcValue>, EngineError> {
    let IpcValue::Array(boxes) = field(req, "boxes") else { return Err(EngineError("script boxes is not an array".into())); };
    let IpcValue::Array(edits) = field(req, "edits") else { return Err(EngineError("script edits is not an array".into())); };
    let scale = number(field(req, "fitWidthPx")) / (width / 9525.0);
    let to_emu = |value: &IpcValue| IpcValue::Number(jscompat::math_round(number(value) / scale * 9525.0));
    let target = |id: &IpcValue| object([("slide", field(req, "slideIndex").clone()), ("el", id.clone())]);
    let mut sorted: Vec<_> = boxes.iter().collect();
    sorted.sort_by_key(|item| truthy(field(item, "groupId")));
    let mut ops = Vec::new();
    for item in sorted {
        let group = field(item, "groupId");
        let bounds = object([("x", to_emu(field(item, "x"))), ("y", to_emu(field(item, "y"))),
            ("cx", to_emu(field(item, "w"))), ("cy", to_emu(field(item, "h")))]);
        let mut fields = vec![("op".into(), IpcValue::String("setTransform".into())),
            ("target".into(), target(field(item, "id")))];
        if truthy(group) { fields.push(("group".into(), group.clone())); fields.push(("absBox".into(), bounds)); }
        else { fields.push(("box".into(), bounds)); }
        fields.push(("rotDeg".into(), field(item, "rotation").clone()));
        ops.push(IpcValue::Object(fields));
    }
    for edit in edits {
        let target = target(field(edit, "id"));
        let group = field(edit, "groupId");
        let mut push = |name: &str, key: &str, value: IpcValue| {
            let mut fields = vec![("op".into(), IpcValue::String(name.into())), ("target".into(), target.clone()), (key.into(), value)];
            if truthy(group) { fields.push(("group".into(), group.clone())); }
            ops.push(IpcValue::Object(fields));
        };
        match field(edit, "kind") {
            IpcValue::String(kind) if kind == "text" => push("setText", "paragraphs", field(edit, "paragraphs").clone()),
            IpcValue::String(kind) if kind == "style" => {
                let style = field(edit, "style");
                let IpcValue::Object(fields) = style else { return Err(EngineError("script style is not an object".into())); };
                let mut font: Vec<_> = fields.iter().filter(|(key, _)| key != "align" && key != "fontSize").cloned().collect();
                let size = field(style, "fontSize");
                if !matches!(size, IpcValue::Undefined) { font.push(("fontSizePt".into(), size.clone())); }
                if !font.is_empty() { push("setFont", "font", IpcValue::Object(font)); }
                let align = field(style, "align");
                if truthy(align) { push("setParagraphFormat", "format", object([("align", align.clone())])); }
            }
            IpcValue::String(kind) if kind == "fill" => push("setFill", "fill", field(edit, "fill").clone()),
            _ => {
                let stroke = field(edit, "stroke");
                let stroke = if truthy(stroke) { object([("color", field(stroke, "color").clone()),
                    ("widthEmu", IpcValue::Number(jscompat::math_round(number(field(stroke, "widthPt")) * 12700.0)))]) }
                    else { IpcValue::Null };
                push("setStroke", "stroke", stroke);
            }
        }
    }
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_precedes_group_geometry_and_style_splits() {
        let item = |id: &str, group: IpcValue| object([
            ("id", IpcValue::String(id.into())), ("groupId", group),
            ("x", IpcValue::Number(1.0)), ("y", IpcValue::Number(2.0)),
            ("w", IpcValue::Number(3.0)), ("h", IpcValue::Number(4.0)),
            ("rotation", IpcValue::Number(0.0)),
        ]);
        let req = object([
            ("fitWidthPx", IpcValue::Number(100.0)), ("slideIndex", IpcValue::Number(0.0)),
            ("boxes", IpcValue::Array(vec![item("child", IpcValue::String("group".into())), item("top", IpcValue::Null)])),
            ("edits", IpcValue::Array(vec![object([
                ("kind", IpcValue::String("style".into())), ("id", IpcValue::String("top".into())),
                ("style", object([("align", IpcValue::String("center".into())), ("fontSize", IpcValue::Number(18.0))])),
            ])])),
        ]);
        let ops = map(&req, 952_500.0).unwrap();
        assert_eq!(ops.len(), 4);
        assert_eq!(field(field(&ops[0], "target"), "el"), &IpcValue::String("top".into()));
        assert_eq!(field(field(&ops[1], "target"), "el"), &IpcValue::String("child".into()));
        assert_eq!(field(field(&ops[1], "absBox"), "cx"), &IpcValue::Number(28_575.0));
        assert_eq!(field(&ops[2], "op"), &IpcValue::String("setFont".into()));
        assert_eq!(field(field(&ops[2], "font"), "fontSizePt"), &IpcValue::Number(18.0));
        assert_eq!(field(&ops[3], "op"), &IpcValue::String("setParagraphFormat".into()));
    }
}
