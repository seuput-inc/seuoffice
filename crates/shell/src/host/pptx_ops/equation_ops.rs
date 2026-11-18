mod omml_linear;

use jscompat::JsNum;
use pptx_engine::insert::{add_element, NewElementOptions};
use pptx_engine::types::{EmuRect, Paragraph, SlideElement, TextAlign, TextBody, TextRun};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;
use super::registry::{field, op_name, record, resolve_element, resolve_slide, string, OpContext};

const M_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/math";

fn latex(op: &IpcValue) -> Result<&str, String> {
    let name = op_name(op);
    let latex = field(op, "latex").and_then(string).filter(|s| !s.trim().is_empty()).ok_or_else(|| format!("op \"{name}\" needs \"latex\": the formula, e.g. \"E = mc^2\" or \"\\\\frac{{a}}{{b}}\"."))?;
    omml::latex_to_omml(latex).map_err(|e| format!("op \"{name}\": \"latex\" uses syntax the converter does not cover ({e}). Supported: \\\\frac, \\\\sqrt, ^, _, \\\\sum/\\\\int with limits, \\\\left( \\\\right), matrices, Greek letters, \\\\text{{}}."))?;
    Ok(latex)
}

fn position(op: &IpcValue) -> Result<bool, String> {
    match field(op, "position") {
        None | Some(IpcValue::Undefined) => Ok(false),
        Some(IpcValue::String(s)) if s == "end" => Ok(false),
        Some(IpcValue::String(s)) if s == "start" => Ok(true),
        _ => Err(format!("op \"{}\": \"position\" must be \"start\" or \"end\" (default end).", op_name(op))),
    }
}
fn optional_box(op: &IpcValue) -> Result<Option<EmuRect>, String> {
    let Some(value) = field(op, "box").filter(|v| !matches!(v, IpcValue::Undefined)) else { return Ok(None); };
    let number = |key| match field(value, key) { Some(IpcValue::Number(n)) if n.is_finite() => Some(*n), _ => None };
    let rect = number("x").zip(number("y")).zip(number("cx").or_else(|| number("w"))).zip(number("cy").or_else(|| number("h")));
    match rect {
        Some((((x, y), cx), cy)) if cx > 0.0 && cy > 0.0 => Ok(Some(EmuRect { x: JsNum(x), y: JsNum(y), cx: JsNum(cx), cy: JsNum(cy) })),
        _ => Err(format!("op \"{}\": \"box\" must be an EMU rect {{x, y, cx, cy}} for the new text box.", op_name(op))),
    }
}
fn equation_run(latex: &str) -> Result<TextRun, String> {
    let inner = omml::latex_to_omml(latex).map_err(|e| e.to_string())?;
    let math = format!("<m:oMath xmlns:m=\"{M_NS}\">{inner}</m:oMath>");
    let linear = omml_linear::omml_to_text(&math);
    let text = if linear.is_empty() { latex.trim().to_owned() } else { linear };
    let escaped = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let raw = format!("<mc:AlternateContent xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\"><mc:Choice xmlns:a14=\"http://schemas.microsoft.com/office/drawing/2010/main\" Requires=\"a14\"><a14:m><m:oMathPara xmlns:m=\"{M_NS}\"><m:oMathParaPr><m:jc m:val=\"centerGroup\"/></m:oMathParaPr>{math}</m:oMathPara></a14:m></mc:Choice><mc:Fallback><a:r><a:rPr lang=\"en-US\" dirty=\"0\"/><a:t>{escaped}</a:t></a:r></mc:Fallback></mc:AlternateContent>");
    let mut run = TextRun::default(); run.text.set(text); run.raw_xml.set(raw); Ok(run)
}
fn validate_inner(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<(), String> {
    latex(op)?; position(op)?;
    if optional_box(op)?.is_some() {
        resolve_slide(ctx, op, false)?;
        if field(op, "target").and_then(|v| field(v, "el")).is_some_and(|v| !matches!(v, IpcValue::Undefined)) {
            return Err(format!("op \"{}\": give either target.el (existing text) or \"box\" (new text box), not both.", op_name(op)));
        }
    } else {
        if field(op, "target").and_then(|v| field(v, "el")).is_none_or(|v| matches!(v, IpcValue::Undefined)) {
            return Err(format!("op \"{}\" needs target.el (a text or shape element) or \"box\": {{x, y, cx, cy}} to create a text box for the formula.", op_name(op)));
        }
        let (target, i) = resolve_element(ctx, op, false)?;
        let element = &ctx.slide(&target).elements.as_deref().unwrap_or(&[])[i];
        let kind = match element { SlideElement::Text(_) => None, SlideElement::Picture(_) => Some("picture"), SlideElement::Group(_) => Some("group"), SlideElement::Table(_) => Some("table"), SlideElement::Chart(_) => Some("chart"), SlideElement::Passthrough(_) => Some("passthrough") };
        if let Some(kind) = kind { return Err(format!("op \"{}\" targets a {kind} element — it only works on text/shape elements.", op_name(op))); }
    }
    Ok(())
}
pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    (op_name(op) == "insertEquation").then(|| validate_inner(op, ctx))
}
pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    if op_name(op) != "insertEquation" { return None; }
    Some((|| {
        let run = equation_run(latex(op)?)?;
        let text = run.text.as_deref().unwrap_or_default().to_owned();
        let start = position(op)?;
        let mut para = Paragraph::default(); para.runs.set(vec![run]); para.align.set(TextAlign::Center);
        let mut result = record(op);
        if let Some(offset) = optional_box(op)? {
            let target = resolve_slide(ctx, op, false)?;
            let opts = NewElementOptions { kind: "textbox".into(), offset, paragraphs: Some(vec![para]), fill_color: None, stroke: None, body_pr: None, adjustments: None, flip_h: None, flip_v: None };
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_millis() as f64;
            let element = add_element(ctx.slide_mut(&target), &opts, &jscompat::random_uuid(), now)?;
            result.created = Some(vec![IpcValue::String(element.id.as_deref().unwrap_or_default().to_owned())]);
            result.after = IpcValue::Object(vec![("text".into(), IpcValue::String(text))]);
        } else {
            let (target, i) = resolve_element(ctx, op, false)?;
            let SlideElement::Text(element) = &mut ctx.slide_mut(&target).elements.as_mut().ok_or("slide has no elements")?[i] else { return Err("equation target is not text".into()); };
            if element.text.is_none() { let mut body = TextBody::default(); body.paragraphs.set(Vec::new()); element.text.set(body); }
            let body = element.text.as_mut().ok_or("missing text body")?;
            if body.paragraphs.is_none() { body.paragraphs.set(Vec::new()); }
            let paragraphs = body.paragraphs.as_mut().ok_or("missing paragraphs")?;
            let only_mark = paragraphs.len() == 1 && paragraphs[0].runs.as_deref().unwrap_or(&[]).iter().all(|r| r.text.as_deref().unwrap_or("").is_empty() && r.field.as_deref().unwrap_or("").is_empty() && r.raw_xml.as_deref().unwrap_or("").is_empty());
            if only_mark { paragraphs[0] = para; } else if start { paragraphs.insert(0, para); } else { paragraphs.push(para); }
            let count = paragraphs.len(); element.dirty.set(true);
            result.after = IpcValue::Object(vec![("text".into(), IpcValue::String(text)), ("paragraphs".into(), IpcValue::Number(count as f64))]);
        }
        Ok(result)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn equation_has_typeset_math_and_readable_fallback() {
        let run = equation_run(r"\frac{a+b}{c}").unwrap();
        assert_eq!(run.text.as_deref(), Some("(a+b)/c"));
        let xml = run.raw_xml.as_deref().unwrap();
        assert!(xml.contains("<mc:Choice"));
        assert!(xml.contains("<m:f>"));
        assert!(xml.contains("<a:t>(a+b)/c</a:t>"));
    }
    #[test]
    fn validates_box_and_position() {
        let op = IpcValue::Object(vec![("op".into(), IpcValue::String("insertEquation".into())), ("box".into(), IpcValue::Object(vec![("x".into(), IpcValue::Number(0.0)), ("y".into(), IpcValue::Number(1.0)), ("w".into(), IpcValue::Number(2.0)), ("h".into(), IpcValue::Number(3.0))]))]);
        assert_eq!(optional_box(&op).unwrap().unwrap().cx.0, 2.0);
        assert!(!position(&op).unwrap());
        let invalid = IpcValue::Object(vec![("box".into(), IpcValue::Null)]);
        assert!(optional_box(&invalid).is_err());
    }
    #[test]
    fn inserts_math_into_new_textbox_and_existing_text() {
        let bytes = pptx_engine::blank::create_blank_pptx(0.0).unwrap();
        let mut opened = pptx_engine::deck::open_pptx(&bytes).unwrap();
        let mut ctx = OpContext { opened: &mut opened, parts: Default::default(), touched_parts: Default::default(), withhold: false };
        let box_op = IpcValue::Object(vec![("op".into(), IpcValue::String("insertEquation".into())), ("latex".into(), IpcValue::String(r"\frac{a}{b}".into())), ("target".into(), IpcValue::Object(vec![("slide".into(), IpcValue::Number(0.0))])), ("box".into(), IpcValue::Object(vec![("x".into(), IpcValue::Number(0.0)), ("y".into(), IpcValue::Number(0.0)), ("cx".into(), IpcValue::Number(10000.0)), ("cy".into(), IpcValue::Number(10000.0))]))]);
        validate(&box_op, &mut ctx).unwrap().unwrap();
        let created = apply(&box_op, &mut ctx).unwrap().unwrap();
        assert_eq!(field(&created.after, "text").and_then(string), Some("a/b"));
        let id = created.created.unwrap().remove(0);
        let append = IpcValue::Object(vec![("op".into(), IpcValue::String("insertEquation".into())), ("latex".into(), IpcValue::String("x^2".into())), ("target".into(), IpcValue::Object(vec![("slide".into(), IpcValue::Number(0.0)), ("el".into(), id)]))]);
        validate(&append, &mut ctx).unwrap().unwrap();
        let edited = apply(&append, &mut ctx).unwrap().unwrap();
        assert_eq!(field(&edited.after, "paragraphs"), Some(&IpcValue::Number(2.0)));
        let xml = pptx_engine::deck::patch_slide_xml(&mut ctx.opened.deck.slides[0]);
        assert!(xml.contains("<m:oMath"));
        assert!(xml.contains("x^2"));
    }
}
