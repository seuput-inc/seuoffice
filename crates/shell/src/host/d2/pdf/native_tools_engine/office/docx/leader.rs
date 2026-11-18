//! Exact-position leader rulings and editable table-of-contents entries.
use serde_json::{json, Value};
use super::super::ir::{Direction, TextBlock};
use super::text::{block_format, paragraph_runs};
fn escape(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }
fn rpr(run: &Value) -> String {
    let mut fonts = Vec::new(); let mut props = Vec::new();
    if let Some(font) = run.get("fontAscii").and_then(Value::as_str) { let font = escape(font); fonts.push(format!("w:ascii=\"{font}\" w:hAnsi=\"{font}\"")); }
    if let Some(font) = run.get("font").and_then(Value::as_str) { fonts.push(format!("w:eastAsia=\"{}\"", escape(font))); }
    if !fonts.is_empty() { props.push(format!("<w:rFonts {}/>", fonts.join(" "))); }
    if run.get("bold").and_then(Value::as_bool) == Some(true) { props.push("<w:b/>".into()); }
    if run.get("italic").and_then(Value::as_bool) == Some(true) { props.push("<w:i/>".into()); }
    if let Some(color) = run.get("color").and_then(Value::as_str) { props.push(format!("<w:color w:val=\"{}\"/>", escape(color))); }
    if let Some(size) = run.get("sizeHalfPoints").and_then(Value::as_f64).filter(|v| *v != 0.0) { props.push(format!("<w:sz w:val=\"{size}\"/><w:szCs w:val=\"{size}\"/>")); }
    if props.is_empty() { String::new() } else { format!("<w:rPr>{}</w:rPr>", props.join("")) }
}
fn raw(run: &Value) -> String { format!("<w:r>{}<w:t xml:space=\"preserve\">{}</w:t></w:r>", rpr(run), escape(run.get("text").and_then(Value::as_str).unwrap_or(""))) }
pub(super) fn style(block: &TextBlock) -> Option<&'static str> {
    if block.lines.len() != 1 || block.toc_entry.is_some() || block.list.is_some() || block.border.is_some() || block.direction == Direction::Rtl || block.lines[0].spans.iter().any(|s| s.note_ref.is_some()) { return None; }
    let text: String = block.lines[0].spans.iter().flat_map(|s| s.text.chars()).filter(|c| !c.is_whitespace()).collect();
    let mut chars = text.chars(); let first = chars.next()?;
    if text.encode_utf16().count() < 8 || chars.any(|c| c != first) { return None; }
    match first { '.' | '…' | '‥' => Some("dot"), '·' => Some("middleDot"), '_' => Some("underscore"), _ => None }
}
pub(super) fn save(block: &TextBlock, leader: Option<&str>, col_base: f64, col_width: f64, page_break: bool, spacing: f64, scale: f64) -> Value {
    let format = block_format(block, scale); let line = format.get("lineRawTwips").and_then(Value::as_f64).unwrap_or(40.0);
    let indent = (block.bounds.x0-col_base).max(0.0);
    let mut tab = ((indent*20.0).round()+36.0).max(((block.bounds.x1-col_base)*20.0).round());
    if block.toc_entry.is_some() { tab = tab.min((col_width*20.0).round()); }
    let style = block.toc_entry.as_ref().map_or(String::new(), |toc| format!("<w:pStyle w:val=\"TOC{}\"/>", toc.level.min(9)));
    let before = if page_break { "<w:pageBreakBefore/>" } else { "" };
    let ind = if indent >= 1.0 { format!("<w:ind w:left=\"{}\"/>", (indent*20.0).round()) } else { String::new() };
    let ppr = format!("<w:pPr>{style}{before}<w:tabs><w:tab w:val=\"right\" w:leader=\"{}\" w:pos=\"{tab}\"/></w:tabs><w:spacing w:before=\"{}\" w:after=\"0\" w:line=\"{line}\" w:lineRule=\"exact\"/>{ind}</w:pPr>", leader.unwrap_or("dot"), (spacing*20.0).round());
    let runs = paragraph_runs(block);
    let body = if let Some(toc) = &block.toc_entry {
        let title = runs.iter().map(raw).collect::<String>();
        let mut number = runs.last().cloned().unwrap_or_else(|| json!({"text":""})); number["text"] = json!(toc.page_number);
        let content = format!("{title}<w:r><w:tab/></w:r>{}", raw(&number));
        let anchor = runs.iter().find_map(|r| r.get("link")?.get("href")?.as_str()?.strip_prefix('#'));
        anchor.map_or(content.clone(), |a| format!("<w:hyperlink w:anchor=\"{}\">{content}</w:hyperlink>", escape(a)))
    } else { format!("<w:r>{}<w:tab/></w:r>", runs.first().map(rpr).unwrap_or_default()) };
    json!({"kind":"xml", "xml":format!("<w:p>{ppr}{body}</w:p>")})
}
