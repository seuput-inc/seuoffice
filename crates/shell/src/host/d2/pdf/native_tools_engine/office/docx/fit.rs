//! Output-font width preflight for rebuilt PDF paragraphs.
use super::super::ir::{Span, TextBlock};
use super::super::script::Script;

fn fullwidth(c: char) -> bool {
    matches!(c as u32, 0x2015 | 0x1100..=0x115f | 0x2e80..=0x9fff | 0xa000..=0xa4cf | 0xac00..=0xd7a3 | 0xf900..=0xfaff | 0xfe30..=0xfe4f | 0xff00..=0xff60 | 0xffe0..=0xffe6 | 0x20000..=0x3fffd)
}

fn width(
    span: &Span,
    text: &str,
    size: f64,
    measure: &impl Fn(&str, &str, f64, bool, bool) -> Option<f64>,
) -> Option<f64> {
    if matches!(span.script, Script::Cjk | Script::Kana | Script::Hangul) {
        return Some(
            text.chars()
                .map(|c| if fullwidth(c) { 1.0 } else { 0.5 })
                .sum::<f64>()
                * size,
        );
    }
    if span.script == Script::Arabic {
        let mut result = if span.font_family.is_empty() {
            None
        } else {
            measure(text, &span.font_family, size, span.bold, span.italic)
        };
        for family in ["Geeza Pro", "Arial", "Times New Roman"] {
            if result.is_some() {
                break;
            }
            result = measure(text, family, size, span.bold, span.italic);
        }
        return result.map(|w| w * 0.75);
    }
    if span.font_family.is_empty() {
        return measure(text, "Arial", size, span.bold, span.italic).map(|w| w * 0.95);
    }
    measure(text, &span.font_family, size, span.bold, span.italic)
}

fn line_width(
    spans: &[Span],
    scale: f64,
    measure: &impl Fn(&str, &str, f64, bool, bool) -> Option<f64>,
) -> Option<(f64, usize)> {
    let mut total = 0.0;
    let mut count = 0;
    for (i, span) in spans.iter().enumerate() {
        if span.note_ref.is_some() || span.invisible {
            continue;
        }
        let text = if i + 1 == spans.len() {
            span.text.trim_end_matches(' ')
        } else {
            &span.text
        };
        if text.is_empty() {
            continue;
        }
        let chars = text.chars().count();
        total += width(span, text, span.font_size * scale, measure)?
            * span.char_scale.unwrap_or(1.0)
            + span.char_spacing_pt.unwrap_or(0.0) * chars as f64;
        count += chars;
    }
    (count > 0).then_some((total, count))
}

pub(super) fn preflight_fit_block(
    block: &mut TextBlock,
    available: f64,
    strict: bool,
    measure: &impl Fn(&str, &str, f64, bool, bool) -> Option<f64>,
) {
    if available <= 0.0
        || block.lines.is_empty()
        || block.toc_entry.is_some()
        || block.list.is_some()
        || block.card_id.is_some()
        || !block.lines.iter().all(|line| {
            line.spans.iter().all(|s| {
                s.note_ref.is_some()
                    || s.invisible
                    || (matches!(
                        s.script,
                        Script::Latin
                            | Script::Common
                            | Script::Arabic
                            | Script::Cjk
                            | Script::Kana
                            | Script::Hangul
                    ) && !s.text.contains('\t'))
            })
        })
    {
        return;
    }
    let font_size = block
        .lines
        .iter()
        .flat_map(|l| &l.spans)
        .fold(1.0_f64, |max, s| max.max(s.font_size));
    let cap = 0.05 * font_size;
    let floor = 6.0_f64.max(0.85 * font_size);
    let target = if strict { 0.99 } else { 0.998 };
    let required = |scale: f64| -> Option<f64> {
        let mut spacing = 0.0_f64;
        for (i, line) in block.lines.iter().enumerate() {
            let (width, count) = line_width(&line.spans, scale, measure)?;
            let avail = (available
                - if i == 0 {
                    block.first_line_indent_pt.max(0.0)
                } else {
                    0.0
                })
                * target;
            if width > avail + 0.25 {
                spacing = spacing.min((avail - width) / count as f64);
            }
        }
        Some(spacing)
    };
    let Some(mut spacing) = required(1.0) else {
        return;
    };
    if spacing == 0.0 {
        return;
    }
    let mut steps = 0;
    while spacing < -cap - 0.05 && steps < 3 && font_size - (steps + 1) as f64 * 0.5 >= floor {
        steps += 1;
        let Some(next) = required((font_size - steps as f64 * 0.5) / font_size) else {
            return;
        };
        spacing = next;
    }
    spacing = (spacing.max(-cap) * 20.0).floor() / 20.0;
    let shrink = steps as f64 * 0.5;
    let scale = (font_size - shrink) / font_size;
    for span in block.lines.iter_mut().flat_map(|line| &mut line.spans) {
        if span.note_ref.is_some() {
            continue;
        }
        if shrink > 0.0 {
            span.font_size = floor.max(span.font_size * scale);
        }
        if spacing < -0.01 {
            span.char_spacing_pt = Some(span.char_spacing_pt.unwrap_or(0.0) + spacing);
        }
    }
}
