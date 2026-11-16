//! Word flow heights and the wrap-risk estimates used by page budgets.
use super::super::ir::{Line, PageBlock, PageSection, TextBlock};
use super::geometry::Geometry;
use super::table::row_heights_pt;
use super::text::block_format;

pub(super) fn is_float(block: &PageBlock) -> bool {
    matches!(block, PageBlock::Image(b) if b.float.is_some())
}
pub(super) fn height(block: &PageBlock, geo: &Geometry, scale: f64) -> f64 {
    match block {
        PageBlock::Text(b) => {
            let twips = if b.border.is_some() && b.lines.is_empty() {
                20.0
            } else {
                block_format(b, scale)
                    .get("lineRawTwips")
                    .and_then(|x| x.as_f64())
                    .unwrap_or(40.0)
            };
            b.lines
                .len()
                .min(b.stitched_from_line.unwrap_or(usize::MAX))
                .max(1) as f64
                * twips
                / 20.0
        }
        PageBlock::Table(b) => row_heights_pt(b).into_iter().flatten().sum::<f64>() * scale,
        PageBlock::Image(b) => {
            let w = b.bounds.width().max(1.0);
            b.bounds.height().max(1.0) * (w.min(geo.content_width_pt) * scale / w)
        }
    }
}
fn max_font(block: &TextBlock) -> f64 {
    block
        .lines
        .iter()
        .flat_map(|l| &l.spans)
        .fold(0.0_f64, |n, s| n.max(s.font_size))
}
pub(super) fn display_risk(block: &TextBlock, width: f64) -> bool {
    max_font(block) >= 28.0
        && block
            .lines
            .iter()
            .map(|l| l.bounds.width())
            .fold(0.0_f64, f64::max)
            >= 0.85 * width.max(1.0)
}
fn brittle(line: &Line) -> bool {
    let mut count = 0;
    let mut cjk = 0;
    for c in line.spans.iter().flat_map(|s| s.text.chars()) {
        if !c.is_whitespace() {
            count += 1;
        }
        if matches!(c as u32, 0x2e80..=0x9fff | 0xf900..=0xfaff | 0xff00..=0xffef | 0x3000..=0x303f)
        {
            cjk += 1;
        }
    }
    count > 0 && cjk as f64 / count as f64 >= 0.5
}
pub(super) fn body_risk(block: &TextBlock, geo: &Geometry) -> bool {
    max_font(block) < 28.0
        && block.lines.len() <= 3
        && block.lines.iter().any(|l| {
            geo.margin_left_pt + geo.content_width_pt - l.bounds.x1 <= 0.015 * geo.content_width_pt
                && l.bounds.width() >= 0.5 * geo.content_width_pt
                && brittle(l)
        })
}
pub(super) fn micro_section(section: &PageSection, geo: &Geometry) -> bool {
    section.columns.len() == 2
        && section.columns.iter().all(|c| {
            c.blocks
                .iter()
                .filter(|b| !is_float(b))
                .map(|b| height(b, geo, 1.0))
                .sum::<f64>()
                <= 48.0
        })
}
pub(super) fn surplus(block: &PageBlock) -> f64 {
    let PageBlock::Text(b) = block else {
        return 0.0;
    };
    if b.lines.is_empty() || max_font(b) > 14.0 {
        return 0.0;
    }
    let twips = block_format(b, 1.0)
        .get("lineRawTwips")
        .and_then(|v| v.as_f64())
        .unwrap_or(40.0);
    (twips
        * b.lines
            .len()
            .min(b.stitched_from_line.unwrap_or(usize::MAX)) as f64
        / 20.0
        - b.bounds.height())
    .max(0.0)
}
pub(super) fn bounds(block: &PageBlock) -> super::super::geometry::Rect {
    match block {
        PageBlock::Text(b) => b.bounds,
        PageBlock::Table(b) => b.bounds,
        PageBlock::Image(b) => b.bounds,
    }
}
pub(super) fn spacing(block: &PageBlock) -> f64 {
    match block {
        PageBlock::Text(b) => b.spacing_before_pt,
        PageBlock::Table(b) => b.spacing_before_pt,
        PageBlock::Image(b) => b.spacing_before_pt,
    }
    .unwrap_or(0.0)
}
