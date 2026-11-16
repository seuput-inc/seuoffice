//! Map highlight fills and thin underline/strike strokes onto source glyphs.
use super::super::geometry::{Rect, median};
use super::super::ir::{PageShapes, PdfChar};
use std::collections::HashSet;

pub(super) struct StyledChars {
    pub chars: Vec<PdfChar>,
    pub warnings: Vec<String>,
    pub consumed_strokes: HashSet<usize>,
}
fn inside(x: f64, y: f64, boxes: &[Rect]) -> bool {
    boxes
        .iter()
        .any(|b| x >= b.x0 - 2.0 && x <= b.x1 + 2.0 && y >= b.y0 - 2.0 && y <= b.y1 + 2.0)
}
fn white(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 6
        && b.iter().enumerate().all(|(i, c)| {
            if i % 2 == 0 {
                c.eq_ignore_ascii_case(&b'F')
            } else {
                c.eq_ignore_ascii_case(&b'F') || c.eq_ignore_ascii_case(&b'E')
            }
        })
}
pub(super) fn apply(
    chars: &[PdfChar],
    shapes: &PageShapes,
    tables: &[Rect],
    page_area: f64,
) -> StyledChars {
    let mut out = chars.to_vec();
    let candidates: Vec<_> = chars
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.is_generated && c.code > 0x20 && c.bounds.width() > 0.0)
        .collect();
    for fill in &shapes.fills {
        if white(&fill.color)
            || page_area > 0.0 && fill.bounds.width() * fill.bounds.height() > 0.5 * page_area
            || inside(fill.bounds.center_x(), fill.bounds.center_y(), tables)
        {
            continue;
        }
        for &(i, c) in &candidates {
            if c.font_size <= 36.0
                && fill.bounds.height() <= 2.5 * c.font_size.max(1.0)
                && c.bounds.overlap_ratio(fill.bounds) >= 0.6
            {
                out[i].highlight = Some(fill.color.clone());
            }
        }
    }
    let mut consumed = HashSet::new();
    let mut mismatch = false;
    for (index, stroke) in shapes.strokes.iter().enumerate() {
        if stroke.orientation != "h"
            || inside(stroke.bounds.center_x(), stroke.bounds.center_y(), tables)
        {
            continue;
        }
        let mut matched = Vec::new();
        for &(i, c) in &candidates {
            let fs = c.font_size.max(1.0);
            if stroke.width_pt > 0.25 * fs {
                continue;
            }
            let overlap = (stroke.bounds.x1.min(c.bounds.x1) - stroke.bounds.x0.max(c.bounds.x0))
                .max(0.0)
                / c.bounds.width();
            if overlap < 0.6 {
                continue;
            }
            let rel = (stroke.bounds.center_y() - c.origin_y) / fs;
            if (0.15..=0.62).contains(&rel) {
                matched.push((i, c, true));
            } else if (-0.35..0.12).contains(&rel) {
                matched.push((i, c, false));
            }
        }
        if matched.is_empty() {
            continue;
        }
        let x0 = matched
            .iter()
            .map(|(_, c, _)| c.bounds.x0)
            .fold(f64::INFINITY, f64::min);
        let x1 = matched
            .iter()
            .map(|(_, c, _)| c.bounds.x1)
            .fold(f64::NEG_INFINITY, f64::max);
        let em = median(
            &matched
                .iter()
                .map(|(_, c, _)| c.font_size)
                .collect::<Vec<_>>(),
        );
        let em = if em == 0.0 { 12.0 } else { em };
        if x0 - stroke.bounds.x0 > 1.5 * em || stroke.bounds.x1 - x1 > 1.5 * em {
            continue;
        }
        for (i, c, strike) in matched {
            if strike {
                out[i].strike = true;
            } else {
                out[i].underline = true;
            }
            mismatch |= c.color != stroke.color;
        }
        consumed.insert(index);
    }
    StyledChars {
        chars: out,
        warnings: if mismatch {
            vec!["underline/strikethrough color differs from text color (not representable, ignored)".into()]
        } else {
            Vec::new()
        },
        consumed_strokes: consumed,
    }
}
