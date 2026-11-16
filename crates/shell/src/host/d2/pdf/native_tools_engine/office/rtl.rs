//! Visual PDF glyph order to logical bidirectional text order.
use super::ir::{Direction, Line, PdfChar};
use super::lines::space;
use super::script::{Script, rtl};
use super::words::{median_char_gap, space_gap_threshold};

pub(super) fn normalize_arabic_forms(chars: &[PdfChar]) -> Vec<PdfChar> {
    chars
        .iter()
        .map(|c| {
            if !matches!(c.code, 0xfb50..=0xfdff | 0xfe70..=0xfeff) {
                return c.clone();
            }
            let normalized = main_core::pdf::font_locate::nfkc(&c.text);
            let text = if normalized.chars().count() > 1 {
                normalized.strip_prefix(' ').unwrap_or(&normalized)
            } else {
                &normalized
            };
            let mut out = c.clone();
            out.text = text.to_owned();
            out
        })
        .collect()
}
pub(super) fn has_rtl(chars: &[PdfChar]) -> bool {
    chars.iter().any(|c| rtl(c.script))
}
fn gap(a: &PdfChar, b: &PdfChar) -> f64 {
    b.loose_box.x0 - a.loose_box.x1
}
fn repair_junk_ligatures(chars: &mut [PdfChar], line_gap: f64) {
    for i in 0..chars.len() {
        let c = &chars[i];
        if c.text.len() != 1 || !c.text.as_bytes()[0].is_ascii_alphabetic() {
            continue;
        }
        let threshold = space_gap_threshold(c.font_size.max(1.0), line_gap);
        let left = i.checked_sub(1).and_then(|j| chars.get(j));
        let right = chars.get(i + 1);
        let left_gap = left.map_or(f64::INFINITY, |l| gap(l, c));
        let right_gap = right.map_or(f64::INFINITY, |r| gap(c, r));
        let matches = |n: Option<&PdfChar>, g: f64| {
            n.is_some_and(|n| {
                n.script == Script::Arabic && n.font_family == c.font_family && g < threshold
            })
        };
        let la = matches(left, left_gap);
        let ra = matches(right, right_gap);
        let lb = left.is_none_or(|n| space(n.code) || left_gap >= threshold);
        let rb = right.is_none_or(|n| space(n.code) || right_gap >= threshold);
        if (la || ra) && (la || lb) && (ra || rb) {
            chars[i].text = "لا".into();
            chars[i].code = 0x644;
            chars[i].script = Script::Arabic;
        }
    }
}
fn same_box(a: &PdfChar, b: &PdfChar) -> bool {
    (a.bounds.x0 - b.bounds.x0).abs() < 0.05
        && (a.bounds.x1 - b.bounds.x1).abs() < 0.05
        && (a.bounds.y0 - b.bounds.y0).abs() < 0.05
        && (a.bounds.y1 - b.bounds.y1).abs() < 0.05
}
fn synthetic_space(x: f64, template: &PdfChar) -> PdfChar {
    let mut c = template.clone();
    c.code = 0x20;
    c.text = " ".into();
    c.bounds.x0 = x;
    c.bounds.x1 = x;
    c.loose_box.x0 = x;
    c.loose_box.x1 = x;
    c.origin_x = x;
    c.is_generated = true;
    c.is_hyphen = false;
    c.script = Script::Common;
    c
}
pub(super) fn reorder_visual_to_logical(chars: &[PdfChar]) -> Vec<PdfChar> {
    let mut visual: Vec<PdfChar> = Vec::new();
    for c in chars {
        if let Some(prev) = visual.last_mut() {
            if !space(prev.code) && !space(c.code) && prev.script == c.script && same_box(prev, c) {
                prev.text.push_str(&c.text);
                continue;
            }
        }
        visual.push(c.clone());
    }
    visual.sort_by(|a, b| {
        a.loose_box
            .center_x()
            .partial_cmp(&b.loose_box.center_x())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let line_gap = median_char_gap(&visual);
    repair_junk_ligatures(&mut visual, line_gap);
    let has_real_space = visual.iter().any(|c| space(c.code) && !c.is_generated);
    let mut spaced: Vec<PdfChar> = Vec::new();
    for c in visual {
        if has_real_space && space(c.code) && c.is_generated {
            continue;
        }
        let inferred = spaced.last().and_then(|prev| {
            (!has_real_space
                && !space(prev.code)
                && !space(c.code)
                && gap(prev, &c)
                    > space_gap_threshold(prev.font_size.min(c.font_size).max(1.0), line_gap))
            .then(|| synthetic_space(prev.loose_box.x1, prev))
        });
        if let Some(space) = inferred {
            spaced.push(space);
        }
        spaced.push(c);
    }
    let mut text = String::new();
    let mut owners = Vec::new();
    for (i, c) in spaced.iter().enumerate() {
        let part = if c.text.is_empty() { " " } else { &c.text };
        for ch in part.chars() {
            let pos = text.len();
            text.push(ch);
            owners.push((pos, i));
        }
    }
    if text.is_empty() {
        return Vec::new();
    }
    let bidi = unicode_bidi::BidiInfo::new(&text, None);
    let levels: Vec<_> = owners.iter().map(|(at, _)| bidi.levels[*at]).collect();
    let order = unicode_bidi::BidiInfo::reorder_visual(&levels);
    let mut seen = vec![false; spaced.len()];
    let mut out = Vec::with_capacity(spaced.len());
    for i in order {
        let owner = owners[i].1;
        if seen[owner] {
            continue;
        }
        seen[owner] = true;
        let mut c = spaced[owner].clone();
        if c.text.encode_utf16().count() == 1 && levels[i].is_rtl() {
            if let Some(m) = c
                .text
                .chars()
                .next()
                .and_then(unicode_bidi_mirroring::get_mirrored)
            {
                c.text = m.to_string();
            }
        }
        out.push(c);
    }
    out
}
pub(super) fn first_strong_dir(lines: &[Line]) -> Direction {
    for line in lines {
        for span in &line.spans {
            if rtl(span.script) {
                return Direction::Rtl;
            }
            if span.script != Script::Common {
                return Direction::Ltr;
            }
        }
    }
    Direction::Ltr
}
