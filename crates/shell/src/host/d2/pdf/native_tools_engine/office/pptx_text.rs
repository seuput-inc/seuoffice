//! Editable slide and table-cell text from analyzed PDF text blocks.
use super::ir::{Alignment, Direction, Span, TextBlock};
use super::script::no_space;
use jscompat::JsNum;
use pptx_engine::types::{Paragraph, TextAlign, TextRun};

fn from_span(span: &Span, scale: f64) -> Option<TextRun> {
    if span.invisible {
        return None;
    }
    let mut r = TextRun::default();
    r.text.set(span.text.clone());
    if span.bold {
        r.bold.set(true);
    }
    if span.italic {
        r.italic.set(true);
    }
    if span.underline {
        r.underline.set(true);
    }
    if span.strike {
        r.strike.set(true);
    }
    if span.font_size > 0.0 {
        r.font_size.set(JsNum(span.font_size * scale));
    }
    if !span.font_family.is_empty() {
        r.font_family.set(span.font_family.clone());
    }
    r.color.set(format!(
        "#{}",
        if span.color.is_empty() {
            "000000"
        } else {
            &span.color
        }
    ));
    if let Some(color) = span.highlight.as_ref().filter(|s| !s.is_empty()) {
        r.highlight.set(format!("#{color}"));
    }
    if let Some(spacing) = span.char_spacing_pt.filter(|v| *v != 0.0) {
        r.letter_spacing.set(JsNum(spacing * scale));
    }
    Some(r)
}
fn same_style(a: &TextRun, b: &TextRun) -> bool {
    *a.bold == *b.bold
        && *a.italic == *b.italic
        && *a.underline == *b.underline
        && *a.strike == *b.strike
        && *a.font_size == *b.font_size
        && *a.font_family == *b.font_family
        && *a.color == *b.color
        && *a.highlight == *b.highlight
        && *a.letter_spacing == *b.letter_spacing
}
fn append_text(run: &mut TextRun, text: &str) {
    if let Some(value) = run.text.as_mut() {
        value.push_str(text);
    }
}
fn push_run(runs: &mut Vec<TextRun>, run: TextRun) {
    if let Some(last) = runs.last_mut() {
        if same_style(last, &run) {
            append_text(last, run.text.as_deref().unwrap_or(""));
            return;
        }
    }
    runs.push(run);
}
pub(super) fn block_runs(block: &TextBlock, scale: f64, hard_breaks: bool) -> Vec<TextRun> {
    let mut runs: Vec<TextRun> = Vec::new();
    for (i, line) in block.lines.iter().enumerate() {
        if i > 0 {
            let prev = &block.lines[i - 1];
            if hard_breaks || line.hard_break_before {
                if let Some(last) = runs.last_mut() {
                    append_text(last, "\n");
                } else {
                    let mut r = TextRun::default();
                    r.text.set("\n".into());
                    runs.push(r);
                }
            } else if prev.ends_with_hyphen && !runs.is_empty() {
                if let Some(text) = runs.last_mut().and_then(|r| r.text.as_mut()) {
                    if matches!(text.chars().next_back(), Some('-' | '\u{ad}' | '\u{2010}')) {
                        text.pop();
                    }
                }
            } else if let (Some(last), Some(previous), Some(next)) =
                (runs.last_mut(), prev.spans.last(), line.spans.first())
            {
                if !no_space(previous.script) && !no_space(next.script) {
                    append_text(last, " ");
                }
            }
        }
        for span in &line.spans {
            if let Some(run) = from_span(span, scale) {
                push_run(&mut runs, run);
            }
        }
    }
    runs.retain(|r| r.text.as_deref().is_some_and(|s| !s.is_empty()));
    runs
}
pub(super) fn text_block_paragraph(block: &TextBlock, scale: f64, hard_breaks: bool) -> Paragraph {
    let mut runs = block_runs(block, scale, hard_breaks);
    if let (Some(info), Some(first)) = (&block.list, runs.first()) {
        let mut marker = first.clone();
        marker.text.set(format!(
            "{} ",
            info.marker.trim_end_matches(jscompat::is_js_space)
        ));
        runs.insert(0, marker);
    }
    if let (Some(toc), Some(last)) = (&block.toc_entry, runs.last()) {
        let mut page = last.clone();
        page.text.set(format!("\t{}", toc.page_number));
        runs.push(page);
    }
    let mut p = Paragraph::default();
    p.runs.set(runs);
    match block.align {
        Alignment::Left => {}
        Alignment::Center => p.align.set(TextAlign::Center),
        Alignment::Right => p.align.set(TextAlign::Right),
        Alignment::Justify => p.align.set(TextAlign::Justify),
    }
    if block.direction == Direction::Rtl {
        p.rtl.set(true);
        if p.align.is_none() {
            p.align.set(TextAlign::Right);
        }
    }
    if !block.lines.is_empty() {
        let pitch = block.bounds.height() / block.lines.len() as f64 * scale;
        if pitch > 0.0 {
            p.line_exact.set(JsNum(pitch));
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::super::geometry::Rect;
    use super::super::ir::Line;
    use super::super::script::Script;
    use super::*;
    fn span(text: &str, script: Script) -> Span {
        Span {
            text: text.into(),
            bounds: Rect::default(),
            font_size: 12.0,
            font_family: "Arial".into(),
            bold: false,
            italic: false,
            color: "000000".into(),
            direction: Direction::Ltr,
            script,
            highlight: None,
            underline: false,
            strike: false,
            char_scale: None,
            char_spacing_pt: None,
            note_ref: None,
            invisible: false,
            href: None,
        }
    }
    fn block(a: Span, b: Span, hyphen: bool) -> TextBlock {
        let line = |span, ends| Line {
            spans: vec![span],
            bounds: Rect::default(),
            baseline: 0.0,
            ends_with_hyphen: ends,
            hard_break_before: false,
        };
        TextBlock {
            lines: vec![line(a, hyphen), line(b, false)],
            bounds: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 40.0,
                y1: 20.0,
            },
            align: Alignment::Left,
            first_line_indent_pt: 0.0,
            direction: Direction::Ltr,
            list: None,
            stitched_from_line: None,
            toc_entry: None,
            spacing_before_pt: None,
            border: None,
            card_id: None,
        }
    }
    #[test]
    fn line_join_distinguishes_hyphens_cjk_and_slide_breaks() {
        let hyphen = block(
            span("inter-", Script::Latin),
            span("national", Script::Latin),
            true,
        );
        assert_eq!(
            block_runs(&hyphen, 1.0, false)[0].text.as_deref(),
            Some("international")
        );
        assert_eq!(
            block_runs(&hyphen, 1.0, true)[0].text.as_deref(),
            Some("inter-\nnational")
        );
        let cjk = block(span("中", Script::Cjk), span("文", Script::Cjk), false);
        assert_eq!(
            block_runs(&cjk, 1.0, false)[0].text.as_deref(),
            Some("中文")
        );
    }
}
