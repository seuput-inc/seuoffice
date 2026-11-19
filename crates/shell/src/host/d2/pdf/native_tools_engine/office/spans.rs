//! Reconstruct styled runs and distinguish tracking from PDF font-metric artifacts.
use super::geometry::{Rect, median};
use super::ir::{Direction, PdfChar, Span};
use super::script::{Script, east_asian, rtl};
use super::words::Word;

pub(super) fn bold(c: &PdfChar) -> bool {
    c.font_weight >= 600.0
        || jscompat::js_regex!(r"bold|black|heavy|semibold|demibold|\bdemi\b", "i")
            .test(&c.font_family)
}
fn same(a: &PdfChar, b: &PdfChar) -> bool {
    a.font_family == b.font_family
        && (a.font_size - b.font_size).abs() <= 0.1
        && bold(a) == bold(b)
        && a.italic == b.italic
        && a.color == b.color
        && a.highlight == b.highlight
        && a.underline == b.underline
        && a.strike == b.strike
        && a.invisible == b.invisible
        && a.href == b.href
}
fn span(c: &PdfChar, script: Script, text: String, bounds: Rect) -> Span {
    Span {
        text,
        bounds,
        font_size: c.font_size,
        font_family: c.font_family.clone(),
        bold: bold(c),
        italic: c.italic,
        color: c.color.clone(),
        direction: if rtl(script) {
            Direction::Rtl
        } else {
            Direction::Ltr
        },
        script,
        highlight: c.highlight.clone(),
        underline: c.underline,
        strike: c.strike,
        char_scale: None,
        char_spacing_pt: None,
        note_ref: c.note_ref.clone(),
        invisible: c.invisible,
        href: c.href.clone(),
    }
}
struct Open<'a> {
    anchor: &'a PdfChar,
    last: &'a PdfChar,
    text: String,
    bounds: Rect,
    script: Script,
    scales: Vec<f64>,
    tracking: Vec<f64>,
    ea_advances: Vec<f64>,
    space_ink: Vec<f64>,
    intra_ink: Vec<f64>,
    space_advances: Vec<f64>,
}
fn flush(open: &mut Option<Open<'_>>, out: &mut Vec<Span>) {
    let Some(o) = open.take() else {
        return;
    };
    let mut s = span(o.anchor, o.script, o.text, o.bounds);
    let scale = median(&o.scales);
    if scale.is_finite() && (0.01..=6.0).contains(&scale) && (scale - 1.0).abs() >= 0.015 {
        s.char_scale = Some(scale);
    }
    if o.tracking.len() >= 2 {
        let tracking = median(&o.tracking);
        let mut artifact = false;
        if tracking < 0.0 && (!o.space_ink.is_empty() || !o.space_advances.is_empty()) {
            let em = o.anchor.font_size.max(1.0);
            let extreme = tracking <= -0.15 * em;
            let space_gap = if o.space_ink.is_empty() {
                f64::NEG_INFINITY
            } else {
                median(&o.space_ink)
            };
            let overhang = if o.intra_ink.is_empty() {
                0.0
            } else {
                median(&o.intra_ink).min(0.0)
            };
            let healthy = !o.space_advances.is_empty() && median(&o.space_advances) >= 0.18 * em;
            artifact = space_gap >= 0.18 * em
                || (extreme && (healthy || space_gap - overhang >= 0.18 * em));
        }
        if tracking < 0.0
            && !artifact
            && o.ea_advances.len() >= 2
            && median(&o.ea_advances) >= 0.9 * o.anchor.font_size.max(1.0)
        {
            artifact = true;
        }
        if !artifact && tracking.is_finite() && tracking.abs() >= 0.05 && tracking.abs() <= 720.0 {
            s.char_spacing_pt = Some(tracking);
        }
    }
    out.push(s);
}
pub(super) fn build_spans(words: &[Word]) -> Vec<Span> {
    let mut result = Vec::new();
    let mut open: Option<Open<'_>> = None;
    let mut previous: Option<&PdfChar> = None;
    for word in words {
        if word.space_before {
            if let Some(o) = open.as_mut() {
                o.text.push(' ');
                if let Some(first) = word.chars.iter().find(|c| !c.text.is_empty()) {
                    o.space_ink.push(first.bounds.x0 - o.last.bounds.x1);
                }
                if let Some(advance) = word.space_advance_pt.filter(|v| *v > 0.0) {
                    o.space_advances.push(advance);
                }
            }
            previous = None;
        }
        for c in &word.chars {
            if c.note_ref.is_some() {
                flush(&mut open, &mut result);
                result.push(span(c, c.script, c.text.clone(), c.bounds));
                previous = None;
                continue;
            }
            if let Some(o) = &open
                && (!same(o.last, c)
                    || (c.script != Script::Common
                        && o.script != Script::Common
                        && c.script != o.script))
            {
                flush(&mut open, &mut result);
                previous = None;
            }
            let o = open.get_or_insert_with(|| Open {
                anchor: c,
                last: c,
                text: String::new(),
                bounds: c.bounds,
                script: c.script,
                scales: Vec::new(),
                tracking: Vec::new(),
                ea_advances: Vec::new(),
                space_ink: Vec::new(),
                intra_ink: Vec::new(),
                space_advances: Vec::new(),
            });
            if o.script == Script::Common && c.script != Script::Common {
                o.script = c.script;
            }
            o.text.push_str(&c.text);
            o.bounds = o.bounds.union(c.bounds);
            o.scales.push(c.hscale.unwrap_or(1.0));
            if let Some(p) = previous {
                let advance = c.origin_x - p.origin_x;
                let nominal = p.loose_box.width();
                if advance > 0.0 && advance <= 2.0 * p.font_size && nominal > 0.0 {
                    o.tracking.push(advance - nominal);
                    if east_asian(p.script) {
                        o.ea_advances.push(advance);
                    }
                    if c.bounds.width() > 0.0 && p.bounds.width() > 0.0 {
                        o.intra_ink.push(c.bounds.x0 - p.bounds.x1);
                    }
                }
            }
            previous = Some(c);
            o.last = c;
        }
    }
    flush(&mut open, &mut result);
    result
}
