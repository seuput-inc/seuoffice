//! Character clustering and visual-line reconstruction.
use super::geometry::{Rect, median};
use super::ir::PdfChar;
use super::script::combining;

#[derive(Debug, Clone)]
pub(super) struct RawLine { pub chars: Vec<PdfChar>, pub baseline: f64, pub bounds: Rect, pub ends_with_hyphen: bool }
pub(super) fn space(code: u32) -> bool { matches!(code, 0x20 | 0xa0 | 0x09 | 0x3000) }
fn visible(c: &PdfChar) -> bool { !space(c.code) && c.code > 0x1f }
pub(super) fn cluster_combining_marks(chars: &[PdfChar]) -> Vec<PdfChar> {
    let mut out: Vec<PdfChar> = Vec::new();
    for c in chars {
        if let Some(prev) = out.last_mut() { if combining(c.code) && visible(prev) {
            prev.text.push_str(&c.text); if c.bounds.area() > 0.0 { prev.bounds = prev.bounds.union(c.bounds); } continue;
        } }
        out.push(c.clone());
    }
    out
}
struct OpenLine { chars: Vec<PdfChar>, baseline: f64, bounds: Option<Rect>, last_visible: Option<PdfChar> }
fn finalize(open: OpenLine) -> Option<RawLine> {
    let visible_chars: Vec<_> = open.chars.iter().filter(|c| visible(c)).collect();
    if visible_chars.is_empty() { return None; }
    let bounds = visible_chars.iter().map(|c| c.bounds).reduce(Rect::union)?;
    let baseline = median(&visible_chars.iter().map(|c| c.origin_y).collect::<Vec<_>>());
    let ends = open.chars.iter().rev().find(|c| visible(c) || c.code == 2 || c.is_hyphen).is_some_and(|c| c.is_hyphen || c.code == 2);
    Some(RawLine { chars: open.chars, baseline, bounds, ends_with_hyphen: ends })
}
fn on_baseline(open: &OpenLine, c: &PdfChar) -> bool {
    let Some(anchor) = &open.last_visible else { return true; };
    let tolerance = 0.45 * c.font_size.max(anchor.font_size).max(1.0);
    (c.origin_y-anchor.origin_y).abs() <= tolerance || (c.origin_y-open.baseline).abs() <= tolerance
}
fn same_line(open: &OpenLine, c: &PdfChar) -> bool {
    let Some(anchor) = &open.last_visible else { return true; };
    let overlaps = open.bounds.is_some_and(|bounds| c.bounds.area() > 0.0 && c.bounds.vertical_overlap_ratio(bounds) >= 0.5);
    (on_baseline(open,c) || overlaps) && c.origin_x >= anchor.origin_x - 1.5*c.font_size.max(anchor.font_size).max(1.0)
}
fn flush(open: &mut Option<OpenLine>, lines: &mut Vec<RawLine>) { if let Some(line) = open.take().and_then(finalize) { lines.push(line); } }
pub(super) fn group_into_lines(chars: &[PdfChar]) -> Vec<RawLine> {
    let mut lines = Vec::new(); let mut open: Option<OpenLine> = None; let mut pending_break = false; let mut pending_generated = false;
    for c in chars {
        if c.code == 0 { continue; }
        if matches!(c.code, 0x0a | 0x0d) { if c.is_generated { pending_generated = true; } else { pending_break = true; } continue; }
        if open.is_none() && space(c.code) { continue; }
        if let Some(current) = &open {
            let must_flush = if visible(c) { pending_break || (pending_generated && !on_baseline(current,c)) || !same_line(current,c) } else { pending_break };
            if must_flush { flush(&mut open,&mut lines); }
        }
        if visible(c) || open.is_none() { pending_break = false; pending_generated = false; }
        let current = open.get_or_insert_with(|| OpenLine { chars: Vec::new(), baseline: c.origin_y, bounds: None, last_visible: None });
        current.chars.push(c.clone());
        if visible(c) { current.bounds = Some(current.bounds.map_or(c.bounds, |r| r.union(c.bounds))); current.last_visible = Some(c.clone()); current.baseline = (current.baseline+c.origin_y)/2.0; }
    }
    flush(&mut open,&mut lines);
    for i in 0..lines.len() {
        if !lines[i].ends_with_hyphen { continue; }
        let marker = lines[i].chars.iter().rposition(|c| c.code == 2 || visible(c)).filter(|&j| lines[i].chars[j].code == 2);
        let next_upper = lines.get(i+1).and_then(|line| line.chars.iter().find(|c| visible(c))).is_some_and(|c| jscompat::js_regex!(r"^\p{Lu}", "u").test(&c.text));
        if let Some(at) = marker.filter(|_| next_upper) { let c = &mut lines[i].chars[at]; c.code = 0x2d; c.text = "-".into(); let bounds = c.bounds; if bounds.area() > 0.0 { lines[i].bounds = lines[i].bounds.union(bounds); } lines[i].ends_with_hyphen = false; }
    }
    lines
}

#[derive(Debug, Clone)]
pub(super) struct IndexedRawLine { pub indices: Vec<usize>, pub baseline: f64, pub bounds: Rect, pub ends_with_hyphen: bool }
pub(super) fn group_into_indexed_lines(chars: &[PdfChar]) -> Vec<IndexedRawLine> {
    let mut lines: Vec<IndexedRawLine> = Vec::new(); let mut open: Option<OpenLine> = None; let mut indices = Vec::new(); let mut pending_break = false; let mut pending_generated = false;
    let flush = |open: &mut Option<OpenLine>, indices: &mut Vec<usize>, lines: &mut Vec<IndexedRawLine>| {
        let ids = std::mem::take(indices);
        if let Some(line) = open.take().and_then(finalize) { lines.push(IndexedRawLine { indices: ids, baseline: line.baseline, bounds: line.bounds, ends_with_hyphen: line.ends_with_hyphen }); }
    };
    for (index,c) in chars.iter().enumerate() {
        if c.code == 0 { continue; }
        if matches!(c.code, 0x0a | 0x0d) { if c.is_generated { pending_generated = true; } else { pending_break = true; } continue; }
        if open.is_none() && space(c.code) { continue; }
        if let Some(current) = &open {
            let must_flush = if visible(c) { pending_break || (pending_generated && !on_baseline(current,c)) || !same_line(current,c) } else { pending_break };
            if must_flush { flush(&mut open,&mut indices,&mut lines); }
        }
        if visible(c) || open.is_none() { pending_break = false; pending_generated = false; }
        let current = open.get_or_insert_with(|| OpenLine { chars: Vec::new(), baseline: c.origin_y, bounds: None, last_visible: None });
        current.chars.push(c.clone()); indices.push(index);
        if visible(c) { current.bounds = Some(current.bounds.map_or(c.bounds, |r| r.union(c.bounds))); current.last_visible = Some(c.clone()); current.baseline = (current.baseline+c.origin_y)/2.0; }
    }
    flush(&mut open,&mut indices,&mut lines);
    for i in 0..lines.len() {
        if !lines[i].ends_with_hyphen { continue; }
        let marker = lines[i].indices.iter().rev().map(|&j| &chars[j]).find(|c| c.code == 2 || visible(c)).filter(|c| c.code == 2);
        let next_upper = lines.get(i+1).and_then(|line| line.indices.iter().map(|&j| &chars[j]).find(|c| visible(c))).is_some_and(|c| jscompat::js_regex!(r"^\p{Lu}", "u").test(&c.text));
        if let Some(c) = marker.filter(|_| next_upper) { if c.bounds.area() > 0.0 { lines[i].bounds = lines[i].bounds.union(c.bounds); } lines[i].ends_with_hyphen = false; }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::script::Script;
    fn character(code: u32, x: f64, y: f64, generated: bool) -> PdfChar {
        PdfChar { code, text: char::from_u32(code).unwrap_or('\u{fffd}').to_string(), bounds: Rect { x0: x, y0: y, x1: x+4.0, y1: y+8.0 }, loose_box: Rect { x0: x, y0: y, x1: x+4.0, y1: y+8.0 }, origin_x: x, origin_y: y, angle: 0.0, font_size: 10.0, font_weight: 400.0, font_family: "Helvetica".into(), italic: false, color: "000000".into(), hscale: None, invisible: false, is_generated: generated, pdf_char_index: None, text_object_id: None, is_hyphen: false, script: Script::Latin, highlight: None, underline: false, strike: false, note_ref: None, href: None }
    }
    #[test]
    fn generated_break_only_splits_when_baseline_changes() {
        let chars = vec![character('a' as u32, 0.0, 20.0, false), character(10, 4.0, 20.0, true), character('b' as u32, 5.0, 20.0, false), character(10, 9.0, 20.0, true), character('c' as u32, 0.0, 0.0, false)];
        let lines = group_into_lines(&chars);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].chars.iter().map(|c| c.text.as_str()).collect::<String>(), "ab");
        assert_eq!(lines[1].chars[0].text, "c");
    }
    #[test]
    fn authored_hyphen_before_capital_is_restored() {
        let mut marker = character(2, 4.0, 20.0, false); marker.is_hyphen = true;
        let chars = vec![character('x' as u32, 0.0, 20.0, false), marker, character(10, 5.0, 20.0, false), character('A' as u32, 0.0, 0.0, false)];
        let lines = group_into_lines(&chars);
        assert!(!lines[0].ends_with_hyphen);
        assert_eq!(lines[0].chars.last().unwrap().text, "-");
    }
    #[test]
    fn indexed_lines_keep_duplicate_characters_distinct() {
        let chars = vec![character('a' as u32, 0.0, 20.0, false), character('a' as u32, 0.0, 20.0, false), character(10, 0.0, 20.0, false), character('b' as u32, 0.0, 0.0, false)];
        let indexed = group_into_indexed_lines(&chars);
        let ordinary = group_into_lines(&chars);
        assert_eq!(indexed.iter().map(|l| &l.indices).collect::<Vec<_>>(), vec![&vec![0,1],&vec![3]]);
        assert_eq!(indexed.iter().map(|l| l.bounds).collect::<Vec<_>>(), ordinary.iter().map(|l| l.bounds).collect::<Vec<_>>());
    }
}
