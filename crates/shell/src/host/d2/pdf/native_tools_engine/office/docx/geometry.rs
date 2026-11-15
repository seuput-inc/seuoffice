//! Document-wide page geometry inferred from content bounds.
use serde_json::{json, Map, Value};
use super::super::ir::{IrPage, PageBlock};

pub(super) struct Geometry {
    pub section: Map<String, Value>,
    pub content_width_pt: f64,
    pub margin_left_pt: f64,
    pub margin_right_pt: f64,
    pub margin_top_pt: f64,
    pub margin_bottom_pt: f64,
}

fn clamp_margin(value: f64, max: f64) -> f64 {
    if value.is_finite() { value.max(12.0).min(max) } else { 72.0 }
}

pub(super) fn compute(pages: &[IrPage]) -> Geometry {
    let width = pages.first().map_or(612.0, |p| if p.width_pt == 0.0 { 612.0 } else { p.width_pt });
    let height = pages.first().map_or(792.0, |p| if p.height_pt == 0.0 { 792.0 } else { p.height_pt });
    let (mut left, mut right, mut top, mut bottom) = (f64::INFINITY, f64::INFINITY, f64::INFINITY, f64::INFINITY);
    for page in pages {
        for block in &page.blocks {
            let (bounds, edge_float) = match block {
                PageBlock::Text(b) => (b.bounds, false),
                PageBlock::Table(b) => (b.bounds, false),
                PageBlock::Image(b) => (b.bounds, b.float.is_some() && (b.bounds.y1 <= 0.12 * page.height_pt || b.bounds.y0 >= 0.88 * page.height_pt)),
            };
            if !edge_float {
                if bounds.x0 >= 0.0 { left = left.min(bounds.x0); }
                if bounds.x1 <= page.width_pt { right = right.min(page.width_pt - bounds.x1); }
            }
            top = top.min(page.height_pt - bounds.y1);
            bottom = bottom.min(bounds.y0);
        }
    }
    let h_max = 108.0_f64.max(width * 0.48);
    let mut left = clamp_margin(left, h_max);
    let mut right = clamp_margin(right, h_max);
    if left > 108.0 { right = 12.0_f64.max(right - 24.0); }
    else if right > 108.0 { left = 12.0_f64.max(left - 24.0); }
    let top = clamp_margin(top, 108.0);
    let bottom = 20.0_f64.max(clamp_margin(bottom, 108.0) - 24.0);
    let section = json!({"pageWidth": (width * 20.0).round(), "pageHeight": (height * 20.0).round(), "orientation": if width > height { "landscape" } else { "portrait" }, "marginTop": (top * 20.0).round(), "marginRight": (right * 20.0).round(), "marginBottom": (bottom * 20.0).round(), "marginLeft": (left * 20.0).round(), "pageBorder": false, "columns": 1}).as_object().cloned().unwrap();
    Geometry { section, content_width_pt: 72.0_f64.max(width - left - right), margin_left_pt: left, margin_right_pt: right, margin_top_pt: top, margin_bottom_pt: bottom }
}
