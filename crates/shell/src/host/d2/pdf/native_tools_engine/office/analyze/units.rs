//! Split visual lines at column-scale gaps and cluster units into rows.
use super::super::geometry::{Rect, median};
use super::super::ir::PdfChar;
use super::super::lines::{RawLine, space};
use super::super::words::{group_into_words, letter_spaced, median_char_gap, space_gap_threshold};

#[derive(Debug, Clone)]
pub(super) struct LineUnit {
    pub id: usize,
    pub chars: Vec<PdfChar>,
    pub bounds: Rect,
    pub baseline: f64,
    pub word_count: usize,
    pub font_size: f64,
}
#[derive(Debug, Clone)]
pub(super) struct UnitRow {
    pub units: Vec<LineUnit>,
    pub bounds: Rect,
    pub baseline: f64,
}
fn visible(c: &PdfChar) -> bool {
    !space(c.code) && c.code > 0x1f
}
fn unit(chars: Vec<PdfChar>, id: usize) -> LineUnit {
    let visible: Vec<_> = chars.iter().filter(|c| visible(c)).collect();
    let bounds = visible
        .iter()
        .map(|c| c.bounds)
        .reduce(Rect::union)
        .unwrap_or_default();
    let baseline = median(&visible.iter().map(|c| c.origin_y).collect::<Vec<_>>());
    let size = median(&visible.iter().map(|c| c.font_size).collect::<Vec<_>>());
    let word_count = group_into_words(&chars, true).len();
    LineUnit {
        id,
        chars,
        bounds,
        baseline,
        word_count,
        font_size: if size == 0.0 { 12.0 } else { size },
    }
}
pub(super) fn column_gap_threshold(size: f64, gap: f64) -> f64 {
    (2.0 * space_gap_threshold(size, gap)).max(0.9 * size)
}
pub(super) fn split_into_units(lines: &[RawLine]) -> Vec<LineUnit> {
    let mut units = Vec::new();
    for raw in lines {
        let mut sorted = raw.chars.clone();
        sorted.sort_by(|a, b| a.loose_box.x0.total_cmp(&b.loose_box.x0));
        let visible_chars: Vec<_> = sorted.iter().filter(|c| visible(c)).cloned().collect();
        if visible_chars.is_empty() {
            continue;
        }
        let size = median(
            &visible_chars
                .iter()
                .map(|c| c.font_size)
                .collect::<Vec<_>>(),
        );
        let size = if size == 0.0 { 12.0 } else { size };
        let threshold = if letter_spaced(&sorted) {
            f64::INFINITY
        } else {
            column_gap_threshold(size, median_char_gap(&visible_chars))
        };
        let mut current = Vec::new();
        let mut last: Option<PdfChar> = None;
        for c in sorted {
            if !visible(&c) {
                if !current.is_empty()
                    && last
                        .as_ref()
                        .is_none_or(|p| c.bounds.center_x() > p.bounds.x1 - 0.1)
                {
                    current.push(c);
                }
                continue;
            }
            if last
                .as_ref()
                .is_some_and(|p| c.loose_box.x0 - p.loose_box.x1 > threshold)
            {
                let id = units.len();
                units.push(unit(std::mem::take(&mut current), id));
            }
            last = Some(c.clone());
            current.push(c);
        }
        if !current.is_empty() {
            let id = units.len();
            units.push(unit(current, id));
        }
    }
    units
}
pub(super) fn cluster_unit_rows(units: &[LineUnit]) -> Vec<UnitRow> {
    let mut sorted = units.to_vec();
    sorted.sort_by(|a, b| b.baseline.total_cmp(&a.baseline));
    let mut rows: Vec<UnitRow> = Vec::new();
    for u in sorted {
        if let Some(row) = rows
            .last_mut()
            .filter(|r| (u.baseline - r.baseline).abs() <= 0.45 * u.font_size.max(1.0))
        {
            row.bounds = row.bounds.union(u.bounds);
            row.units.push(u);
        } else {
            rows.push(UnitRow {
                bounds: u.bounds,
                baseline: u.baseline,
                units: vec![u],
            });
        }
    }
    for row in &mut rows {
        row.units
            .sort_by(|a, b| a.bounds.x0.total_cmp(&b.bounds.x0));
    }
    rows
}
