//! Translate PDF text spans to docx-engine's rich run representation.
use serde_json::{json, Map, Value};
use super::super::ir::{Alignment, Direction, IrPage, Span, TextBlock};
use super::super::script::{east_asian, no_space, Script};
use super::numbering::Numbering;

fn highlight(hex: &str) -> Option<&'static str> {
    if hex.len() != 6 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) { return None; }
    let rgb = [u8::from_str_radix(&hex[0..2], 16).ok()?, u8::from_str_radix(&hex[2..4], 16).ok()?, u8::from_str_radix(&hex[4..6], 16).ok()?];
    const PALETTE: [(&str, [u8; 3]); 16] = [
        ("black", [0,0,0]), ("blue", [0,0,255]), ("cyan", [0,255,255]), ("green", [0,255,0]),
        ("magenta", [255,0,255]), ("red", [255,0,0]), ("yellow", [255,255,0]), ("white", [255,255,255]),
        ("darkBlue", [0,0,139]), ("darkCyan", [0,139,139]), ("darkGreen", [0,100,0]), ("darkMagenta", [139,0,139]),
        ("darkRed", [139,0,0]), ("darkYellow", [128,128,0]), ("darkGray", [128,128,128]), ("lightGray", [192,192,192]),
    ];
    let best = PALETTE.iter().min_by_key(|(_, color)| (0..3).map(|i| (i32::from(rgb[i]) - i32::from(color[i])).pow(2)).sum::<i32>())?.0;
    (best != "white").then_some(best)
}

fn span_run(span: &Span) -> Map<String, Value> {
    let mut run = Map::new();
    run.insert("text".into(), json!(span.text));
    if let Some(id) = &span.note_ref {
        run.insert("noteRef".into(), json!({"kind": "footnote", "id": id}));
        return run;
    }
    for (key, value) in [("bold", span.bold), ("italic", span.italic), ("underline", span.underline), ("strike", span.strike)] {
        if value { run.insert(key.into(), json!(true)); }
    }
    if let Some(color) = span.highlight.as_deref().and_then(highlight) { run.insert("highlight".into(), json!(color)); }
    if !span.color.is_empty() && span.color != "000000" { run.insert("color".into(), json!(span.color)); }
    let half_points = (span.font_size * 2.0).round();
    if half_points.is_finite() && half_points > 0.0 && half_points <= 3276.0 { run.insert("sizeHalfPoints".into(), json!(half_points)); }
    if !span.font_family.is_empty() { run.insert(if east_asian(span.script) { "font" } else { "fontAscii" }.into(), json!(span.font_family)); }
    if span.direction == Direction::Rtl {
        run.insert("rtl".into(), json!(true));
        let family = if span.font_family.is_empty() { if span.script == Script::Hebrew { "David" } else { "Traditional Arabic" } } else { &span.font_family };
        run.insert("fontCs".into(), json!(family));
    }
    if span.invisible { run.insert("vanish".into(), json!(true)); run.insert("vanishOwn".into(), json!(true)); }
    if let Some(spacing) = span.char_spacing_pt { run.insert("charSpacingTwips".into(), json!(jscompat::math_round(spacing * 20.0))); }
    if let Some(scale) = span.char_scale { let pct = (scale * 100.0).round(); if pct != 100.0 { run.insert("charScalePct".into(), json!(pct)); } }
    if let Some(href) = &span.href { run.insert("link".into(), json!({"href": href})); }
    run
}

fn append_text(run: &mut Map<String, Value>, text: &str) {
    let current = run.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
    run.insert("text".into(), Value::String(current + text));
}

/// Explicit paragraph line geometry prevents the blank template's defaults
/// from adding extra leading and after-spacing to every rebuilt paragraph.
pub(super) fn block_format(block: &TextBlock, height_scale: f64) -> Map<String, Value> {
    let native_lines = block.lines.len().min(block.stitched_from_line.unwrap_or(usize::MAX));
    let measured = (block.bounds.height() / native_lines.max(1) as f64 * 20.0).round();
    let max_font = block.lines.iter().take(native_lines).flat_map(|line| &line.spans)
        .fold(1.0_f64, |max, span| max.max(span.font_size));
    let floor = if native_lines <= 1 { (max_font * 1.16 * 20.0).round() } else { 0.0 };
    let line = 40.0_f64.max(measured).max(floor);
    let line = if height_scale < 1.0 { 20.0_f64.max((line * height_scale).round()) } else { line };
    let mut format = Map::new();
    format.insert("lineRule".into(), json!("exact"));
    format.insert("lineRawTwips".into(), json!(line));
    format.insert("spaceAfter".into(), json!(0));
    let alignment = match block.align { Alignment::Left => "left", Alignment::Center => "center", Alignment::Right => "right", Alignment::Justify => "justify" };
    if block.direction == Direction::Rtl {
        format.insert("bidi".into(), json!(true));
        if block.align != Alignment::Right { format.insert("align".into(), json!(alignment)); }
    } else {
        if block.align != Alignment::Left { format.insert("align".into(), json!(alignment)); }
        if block.first_line_indent_pt > 0.0 { format.insert("indentFirstLine".into(), json!((block.first_line_indent_pt * 20.0).round())); }
    }
    format
}

pub(super) fn paragraph_runs(block: &TextBlock) -> Vec<Value> {
    let mut runs: Vec<Map<String, Value>> = Vec::new();
    for (index, line) in block.lines.iter().enumerate() {
        if index > 0 {
            let previous = &block.lines[index - 1];
            if line.hard_break_before {
                if let Some(last) = runs.last_mut().filter(|r| !r.contains_key("noteRef")) { append_text(last, "\n"); }
                else { runs.push(Map::from_iter([("text".into(), json!("\n"))])); }
            } else if previous.ends_with_hyphen {
                if let Some(last) = runs.last_mut() {
                    if let Some(text) = last.get("text").and_then(Value::as_str) {
                        let text = match text.chars().last() {
                            Some(c @ ('-' | '\u{ad}' | '\u{2010}')) => &text[..text.len() - c.len_utf8()],
                            _ => text,
                        }.to_owned();
                        last.insert("text".into(), json!(text));
                    }
                }
            } else if let (Some(last), Some(prev), Some(next)) = (runs.last_mut(), previous.spans.last(), line.spans.first()) {
                if !no_space(prev.script) && !no_space(next.script) { append_text(last, " "); }
            }
        }
        for span in &line.spans {
            let mut run = span_run(span);
            if let Some(last) = runs.last_mut() {
                if !last.contains_key("noteRef") && !run.contains_key("noteRef") {
                    let text = run.remove("text").unwrap_or_default();
                    let previous_text = last.remove("text").unwrap_or_default();
                    if *last == run {
                        last.insert("text".into(), previous_text);
                        append_text(last, text.as_str().unwrap_or_default());
                        continue;
                    }
                    last.insert("text".into(), previous_text);
                    run.insert("text".into(), text);
                }
            }
            runs.push(run);
        }
    }
    runs.into_iter().map(Value::Object).collect()
}

pub(super) fn text_block(block: &TextBlock, page_break: bool, spacing_before: f64, page: usize, numbering: &mut Numbering, height_scale: f64) -> Value {
    let mut format = block_format(block, height_scale);
    if page_break { format.insert("pageBreakBefore".into(), json!(true)); }
    let mut spacing = spacing_before;
    if let Some(border) = &block.border {
        if border.side == "left" {
            format.insert("borders".into(), json!("l"));
            format.insert("borderStyle".into(), json!({"color": border.color, "szEighths": 2.0_f64.max((border.width_pt * 8.0).round()), "spacePt": border.space_pt}));
        } else {
            let gap = if page_break && border.side == "top" { 0.0 } else { 0.0_f64.max(border.space_pt.min(spacing - border.width_pt - 1.0)) };
            format.insert("borders".into(), json!(if border.side == "top" { "t" } else { "b" }));
            format.insert("borderStyle".into(), json!({"color": border.color, "szEighths": 2.0_f64.max((border.width_pt * 8.0).round()), "spacePt": gap}));
            spacing = 0.0_f64.max(spacing - gap - border.width_pt - 1.0);
            if block.lines.is_empty() {
                format.insert("lineRawTwips".into(), json!(20));
                spacing = 0.0_f64.max(spacing - 0.0_f64.max(1.0 + border.width_pt - block.bounds.height()));
            }
            if let Some(indent) = border.indent_right_pt.filter(|v| *v > 0.0) { format.insert("indentRight".into(), json!((indent * 20.0).round())); }
            if let Some(indent) = border.indent_left_pt.filter(|v| *v > 0.0) { format.insert("indentLeft".into(), json!((indent * 20.0).round())); }
        }
    }
    if spacing > 0.0 { format.insert("spaceBefore".into(), json!((spacing * 20.0).round())); }
    let mut generated = Map::new();
    generated.insert("type".into(), json!(if block.list.is_some() { "listItem" } else { "paragraph" }));
    generated.insert("runs".into(), json!(paragraph_runs(block)));
    if let Some(list) = &block.list {
        generated.insert("list".into(), json!({"kind": list.kind, "numId": numbering.id_for(page, list), "ilvl": list.level}));
        format.remove("indentFirstLine");
    }
    generated.insert("format".into(), Value::Object(format));
    json!({"kind": "generated", "block": generated})
}

pub(super) fn canvas_text(block: &TextBlock, page: &IrPage) -> Value {
    let mut format = block_format(block, 1.0);
    let width = block.bounds.width().max(1.0);
    let pad = 4.0_f64.max(width * 0.04);
    let x = block.bounds.x0 - match block.align { Alignment::Center => pad / 2.0, Alignment::Right => pad, _ => 0.0 };
    format.insert("frame".into(), json!({"wTwips": ((width + pad) * 20.0).round(), "xTwips": jscompat::math_round(x * 20.0), "yTwips": jscompat::math_round((page.height_pt - block.bounds.y1) * 20.0)}));
    if let Some(border) = &block.border {
        format.insert("borders".into(), json!(match border.side.as_str() { "top" => "t", "left" => "l", _ => "b" }));
        format.insert("borderStyle".into(), json!({"color": border.color, "szEighths": 2.0_f64.max((border.width_pt * 8.0).round()), "spacePt": border.space_pt}));
        if block.lines.is_empty() { format.insert("lineRawTwips".into(), json!(20)); }
    }
    let mut runs = paragraph_runs(block);
    if let (Some(list), Some(first)) = (&block.list, runs.first()) {
        let mut marker = first.clone();
        marker["text"] = json!(format!("{} ", list.marker.trim_end()));
        runs.insert(0, marker);
    }
    json!({"kind": "generated", "block": {"type": "paragraph", "runs": runs, "format": format}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::super::geometry::Rect;
    use super::super::super::ir::Line;

    fn span(text: &str, script: Script) -> Span {
        Span { text: text.into(), bounds: Rect::default(), font_size: 12.0, font_family: "Arial".into(), bold: false, italic: false, color: "000000".into(), direction: Direction::Ltr, script, highlight: None, underline: false, strike: false, char_scale: None, char_spacing_pt: None, note_ref: None, invisible: false, href: None }
    }
    fn block(lines: Vec<Line>) -> TextBlock {
        TextBlock { lines, bounds: Rect { x0: 0.0, y0: 0.0, x1: 100.0, y1: 24.0 }, align: Alignment::Left, first_line_indent_pt: 0.0, direction: Direction::Ltr, list: None, stitched_from_line: None, toc_entry: None, spacing_before_pt: None, border: None, card_id: None }
    }
    fn line(text: &str, script: Script, hyphen: bool) -> Line {
        Line { spans: vec![span(text, script)], bounds: Rect::default(), baseline: 0.0, ends_with_hyphen: hyphen, hard_break_before: false }
    }
    #[test]
    fn text_joins_hyphens_and_cjk_without_inventing_spaces() {
        let runs = paragraph_runs(&block(vec![line("hy-", Script::Latin, true), line("phen", Script::Latin, false)]));
        assert_eq!(runs[0]["text"], "hyphen");
        let runs = paragraph_runs(&block(vec![line("漢", Script::Cjk, false), line("字", Script::Cjk, false)]));
        assert_eq!(runs[0]["text"], "漢字");
    }
    #[test]
    fn footnote_references_are_atomic() {
        let mut reference = span("1", Script::Latin);
        reference.note_ref = Some("2".into());
        let mut first = line("text", Script::Latin, false);
        first.spans.push(reference);
        first.spans.push(span("tail", Script::Latin));
        let runs = paragraph_runs(&block(vec![first]));
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[1]["noteRef"]["id"], "2");
        assert_eq!(runs[2]["text"], "tail");
    }
}
