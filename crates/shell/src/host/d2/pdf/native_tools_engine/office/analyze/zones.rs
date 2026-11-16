//! Rule-separated, side-by-side text zones and exact stack cells.
use super::super::chars::analyze_chars;
use super::super::geometry::Rect;
use super::super::ir::{Alignment, Direction, PageShapes, TableBlock, TableCell, TextBlock};
use super::units::LineUnit;
use std::collections::HashSet;

struct RuleBand {
    bounds: Rect,
    strokes: Vec<usize>,
    double: bool,
}

pub(super) struct DetectedZones {
    pub tables: Vec<TableBlock>,
    pub remaining_units: Vec<LineUnit>,
    pub consumed_strokes: HashSet<usize>,
}

fn rule_bands(shapes: &PageShapes) -> Vec<RuleBand> {
    let mut vertical: Vec<usize> = shapes
        .strokes
        .iter()
        .enumerate()
        .filter_map(|(i, s)| (s.orientation == "v").then_some(i))
        .collect();
    vertical.sort_by(|&a, &b| {
        shapes.strokes[a]
            .bounds
            .center_x()
            .total_cmp(&shapes.strokes[b].bounds.center_x())
    });
    let mut bands: Vec<RuleBand> = Vec::new();
    for index in vertical {
        let bounds = shapes.strokes[index].bounds;
        if let Some(band) = bands
            .last_mut()
            .filter(|band| bounds.center_x() - band.bounds.center_x() <= 4.0)
        {
            band.bounds = band.bounds.union(bounds);
            band.strokes.push(index);
        } else {
            bands.push(RuleBand {
                bounds,
                strokes: vec![index],
                double: false,
            });
        }
    }
    for band in &mut bands {
        let min = band
            .strokes
            .iter()
            .map(|&i| shapes.strokes[i].bounds.center_x())
            .fold(f64::INFINITY, f64::min);
        let max = band
            .strokes
            .iter()
            .map(|&i| shapes.strokes[i].bounds.center_x())
            .fold(f64::NEG_INFINITY, f64::max);
        band.double = max - min >= 1.5;
    }
    bands
}

fn y_coverage(band: &RuleBand, shapes: &PageShapes) -> f64 {
    let mut spans: Vec<_> = band
        .strokes
        .iter()
        .map(|&i| shapes.strokes[i].bounds)
        .collect();
    spans.sort_by(|a, b| a.y0.total_cmp(&b.y0));
    let mut covered = 0.0;
    let mut cursor = f64::NEG_INFINITY;
    for span in spans {
        let lo = span.y0.max(cursor);
        if span.y1 > lo {
            covered += span.y1 - lo;
        }
        cursor = cursor.max(span.y1);
    }
    let height = band.bounds.height();
    if height > 0.0 { covered / height } else { 0.0 }
}

pub(super) fn build_stack_cell(
    units: &[LineUnit],
    bounds: Rect,
    zone_height: f64,
    allow_v_align_center: bool,
) -> TableCell {
    let mut sorted: Vec<_> = units.iter().collect();
    sorted.sort_by(|a, b| a.bounds.x0.total_cmp(&b.bounds.x0));
    let chars: Vec<_> = sorted
        .into_iter()
        .flat_map(|unit| unit.chars.iter().cloned())
        .collect();
    let mut lines = analyze_chars(&chars);
    lines.sort_by(|a, b| {
        b.baseline
            .total_cmp(&a.baseline)
            .then_with(|| a.bounds.x0.total_cmp(&b.bounds.x0))
    });
    let mut blocks: Vec<_> = lines
        .into_iter()
        .map(|line| TextBlock {
            bounds: line.bounds,
            lines: vec![line],
            align: Alignment::Left,
            first_line_indent_pt: 0.0,
            direction: Direction::Ltr,
            list: None,
            stitched_from_line: None,
            toc_entry: None,
            spacing_before_pt: None,
            border: None,
            card_id: None,
        })
        .collect();
    let min_y = units
        .iter()
        .map(|u| u.bounds.y0)
        .fold(f64::INFINITY, f64::min);
    let max_y = units
        .iter()
        .map(|u| u.bounds.y1)
        .fold(f64::NEG_INFINITY, f64::max);
    let centered = !units.is_empty() && allow_v_align_center && max_y - min_y < 0.85 * zone_height;
    for index in 0..blocks.len() {
        let gap = if index == 0 {
            if centered {
                0.0
            } else {
                bounds.y1 - blocks[index].bounds.y1
            }
        } else {
            blocks[index - 1].bounds.y0 - blocks[index].bounds.y1
        };
        if gap.is_finite() && gap >= 2.0 {
            blocks[index].spacing_before_pt = Some(gap.min(1584.0));
        }
        let inset = blocks[index].bounds.x0 - bounds.x0;
        if inset.is_finite() && inset >= 6.0 {
            blocks[index].first_line_indent_pt = inset.min(1584.0);
        }
    }
    TableCell {
        bounds,
        grid_span: 1,
        v_merge: None,
        fill: None,
        v_align: centered.then(|| "center".to_owned()),
        soft_edges: None,
        blocks,
    }
}

pub(super) fn detect_rule_separated_zones(
    units: &[LineUnit],
    shapes: &PageShapes,
    claimed: &[Rect],
    page_width: f64,
    page_height: f64,
) -> DetectedZones {
    let mut out = DetectedZones {
        tables: Vec::new(),
        remaining_units: units.to_vec(),
        consumed_strokes: HashSet::new(),
    };
    if page_width > page_height {
        return out;
    }
    for band in rule_bands(shapes) {
        let bounds = band.bounds;
        if bounds.width() > 8.0 || bounds.height() < 48.0 || y_coverage(&band, shapes) < 0.7 {
            continue;
        }
        let center = bounds.center_x();
        if center < 0.2 * page_width
            || center > 0.8 * page_width
            || claimed.iter().any(|r| r.intersection_area(bounds) > 0.0)
        {
            continue;
        }
        let mut left = Vec::new();
        let mut right = Vec::new();
        let mut zone_ids = HashSet::new();
        let mut crossing = false;
        for unit in &out.remaining_units {
            let cy = unit.bounds.center_y();
            if cy < bounds.y0 - 3.0 || cy > bounds.y1 + 3.0 {
                continue;
            }
            zone_ids.insert(unit.id);
            if unit.bounds.x1 <= bounds.x0 + 0.5 {
                left.push(unit.clone());
            } else if unit.bounds.x0 >= bounds.x1 - 0.5 {
                right.push(unit.clone());
            } else {
                crossing = true;
            }
        }
        if crossing || left.len() < 2 || right.len() < 2 {
            continue;
        }
        let rows = |side: &[LineUnit]| {
            side.iter()
                .map(|u| (u.baseline + 0.5).floor() as i64)
                .collect::<HashSet<_>>()
                .len()
        };
        if rows(&left) < 2 || rows(&right) < 2 {
            continue;
        }
        let extent = |side: &[LineUnit]| {
            (
                side.iter()
                    .map(|u| u.bounds.y0)
                    .fold(f64::INFINITY, f64::min),
                side.iter()
                    .map(|u| u.bounds.y1)
                    .fold(f64::NEG_INFINITY, f64::max),
            )
        };
        let (llo, lhi) = extent(&left);
        let (rlo, rhi) = extent(&right);
        let height = (lhi - llo).min(rhi - rlo);
        if height <= 0.0 || lhi.min(rhi) - llo.max(rlo) < 0.4 * height {
            continue;
        }
        let Some(content) = left
            .iter()
            .chain(&right)
            .map(|u| u.bounds)
            .reduce(Rect::union)
        else {
            continue;
        };
        let zone = content.union(bounds);
        if zone.height() > 0.75 * page_height || bounds.height() < 0.6 * zone.height() {
            continue;
        }
        let inside = |r: Rect| {
            r.x1 > zone.x0 - 2.0
                && r.x0 < zone.x1 + 2.0
                && r.y1 > zone.y0 - 2.0
                && r.y0 < zone.y1 + 2.0
        };
        if shapes
            .fills
            .iter()
            .chain(&shapes.curved_fills)
            .any(|fill| inside(fill.bounds))
            || shapes
                .strokes
                .iter()
                .enumerate()
                .any(|(i, stroke)| !band.strokes.contains(&i) && inside(stroke.bounds))
        {
            continue;
        }
        let left_box = Rect {
            x0: zone.x0,
            x1: center,
            y0: zone.y0,
            y1: zone.y1,
        };
        let right_box = Rect {
            x0: center,
            x1: zone.x1,
            y0: zone.y0,
            y1: zone.y1,
        };
        out.tables.push(TableBlock {
            bounds: zone,
            col_widths_pt: vec![center - zone.x0, zone.x1 - center],
            rows: vec![vec![
                build_stack_cell(&left, left_box, zone.height(), true),
                build_stack_cell(&right, right_box, zone.height(), true),
            ]],
            confidence: Some(0.9),
            border_color: None,
            form: false,
            separator_rule: Some(if band.double { "double" } else { "single" }.to_owned()),
            spacing_before_pt: None,
        });
        out.consumed_strokes.extend(band.strokes);
        out.remaining_units
            .retain(|unit| !zone_ids.contains(&unit.id));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::super::ir::{PdfChar, Stroke};
    use super::super::super::script::script_of;
    use super::*;

    fn unit(id: usize, x: f64, y: f64) -> LineUnit {
        let bounds = Rect {
            x0: x,
            x1: x + 10.0,
            y0: y,
            y1: y + 10.0,
        };
        let char_ = PdfChar {
            code: 65,
            text: "A".into(),
            bounds,
            loose_box: bounds,
            origin_x: x,
            origin_y: y,
            angle: 0.0,
            font_size: 10.0,
            font_weight: 400.0,
            font_family: "Example".into(),
            italic: false,
            color: "000000".into(),
            hscale: None,
            invisible: false,
            is_generated: false,
            pdf_char_index: None,
            text_object_id: None,
            is_hyphen: false,
            script: script_of(65),
            highlight: None,
            underline: false,
            strike: false,
            note_ref: None,
            href: None,
        };
        LineUnit {
            id,
            chars: vec![char_],
            bounds,
            baseline: y,
            line_index: id,
            word_count: 1,
            font_size: 10.0,
        }
    }

    #[test]
    fn separator_claims_only_its_two_text_stacks() {
        let shapes = PageShapes {
            strokes: vec![Stroke {
                bounds: Rect {
                    x0: 299.5,
                    x1: 300.5,
                    y0: 600.0,
                    y1: 700.0,
                },
                orientation: "v".into(),
                width_pt: 1.0,
                color: "000000".into(),
                from_form: false,
            }],
            fills: Vec::new(),
            curved_fills: Vec::new(),
            ignored_paths: 0,
        };
        let units = vec![
            unit(1, 100.0, 620.0),
            unit(2, 100.0, 670.0),
            unit(3, 350.0, 620.0),
            unit(4, 350.0, 670.0),
            unit(5, 100.0, 500.0),
        ];
        let result = detect_rule_separated_zones(&units, &shapes, &[], 600.0, 800.0);
        assert_eq!(result.tables.len(), 1);
        assert_eq!(result.tables[0].separator_rule.as_deref(), Some("single"));
        assert_eq!(result.consumed_strokes, HashSet::from([0]));
        assert_eq!(
            result
                .remaining_units
                .iter()
                .map(|u| u.id)
                .collect::<Vec<_>>(),
            vec![5]
        );
    }
}
