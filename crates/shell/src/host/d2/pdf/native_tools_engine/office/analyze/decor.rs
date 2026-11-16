//! Assign decorative rules to paragraph borders without disturbing table strokes.
use super::super::{
    geometry::Rect,
    ir::{Alignment, DecorBorder, Direction, PageBlock, PageSection, Stroke, TextBlock},
};
use std::collections::HashSet;

pub(super) struct DecorResult {
    pub ignored_vertical: usize,
    pub dropped_bars: usize,
}
fn bounds(b: &PageBlock) -> Rect {
    match b {
        PageBlock::Text(t) => t.bounds,
        PageBlock::Image(i) => i.bounds,
        PageBlock::Table(t) => t.bounds,
    }
}
fn floating(b: &PageBlock) -> bool {
    matches!(b,PageBlock::Image(i) if i.float.is_some())
}
fn overlap(a: Rect, b: Rect) -> f64 {
    let n = a.width().min(b.width());
    let v = a.x1.min(b.x1) - a.x0.max(b.x0);
    if n > 0.0 && v > 0.0 { v / n } else { 0.0 }
}
fn border(
    s: &Stroke,
    side: &str,
    space: f64,
    left: Option<f64>,
    right: Option<f64>,
) -> DecorBorder {
    DecorBorder {
        side: side.into(),
        color: s.color.clone(),
        width_pt: s.width_pt,
        space_pt: space.clamp(0.0, 31.0),
        indent_left_pt: left,
        indent_right_pt: right,
    }
}
pub(super) fn apply_decor_borders(
    sections: &mut [PageSection],
    strokes: &[Stroke],
    exclude: &[Rect],
    consumed: &HashSet<usize>,
) -> DecorResult {
    let mut ignored = 0;
    let mut standalone = Vec::new();
    for (idx, s) in strokes.iter().enumerate() {
        if consumed.contains(&idx) {
            continue;
        }
        let b = s.bounds;
        let (cx, cy) = (b.center_x(), b.center_y());
        if exclude
            .iter()
            .any(|r| cx >= r.x0 - 2.0 && cx <= r.x1 + 2.0 && cy >= r.y0 - 2.0 && cy <= r.y1 + 2.0)
        {
            continue;
        }
        let len = if s.orientation == "h" {
            b.width()
        } else {
            b.height()
        };
        if s.orientation == "v" {
            let mut best = None;
            let mut gap = f64::INFINITY;
            if s.width_pt <= 6.0 && len >= 14.0 {
                for (si, sec) in sections.iter().enumerate() {
                    for (ci, col) in sec.columns.iter().enumerate() {
                        for (bi, block) in col.blocks.iter().enumerate() {
                            let PageBlock::Text(t) = block else { continue };
                            if t.toc_entry.is_some() || t.border.is_some() {
                                continue;
                            }
                            let d = t.bounds.x0 - b.x1;
                            let y = t.bounds.y1.min(b.y1) - t.bounds.y0.max(b.y0);
                            if (-1.0..=30.0).contains(&d)
                                && d < gap
                                && y >= 0.55 * t.bounds.height()
                                && y >= 0.45 * len
                            {
                                gap = d;
                                best = Some((si, ci, bi));
                            }
                        }
                    }
                }
            }
            if let Some((si, ci, bi)) = best {
                if let PageBlock::Text(t) = &mut sections[si].columns[ci].blocks[bi] {
                    t.border = Some(border(s, "left", gap, None, None));
                }
                continue;
            }
            if s.width_pt <= 3.0 && len >= 30.0 {
                ignored += 1;
            }
            continue;
        }
        if s.width_pt > 3.0 || len < 30.0 {
            continue;
        }
        let (mut below, mut above, mut far) = (None, None, None);
        let (mut bd, mut ad, mut fd) = (f64::INFINITY, f64::INFINITY, f64::INFINITY);
        for (si, sec) in sections.iter().enumerate() {
            for (ci, col) in sec.columns.iter().enumerate() {
                for (bi, block) in col.blocks.iter().enumerate() {
                    let PageBlock::Text(t) = block else { continue };
                    if t.toc_entry.is_some() {
                        continue;
                    }
                    let x = overlap(b, t.bounds);
                    if x < 0.3 {
                        continue;
                    }
                    let db = cy - t.bounds.y1;
                    let da = t.bounds.y0 - cy;
                    if x >= 0.5 {
                        if (-1.0..=24.0).contains(&db) && db < bd {
                            bd = db;
                            below = Some((si, ci, bi));
                        }
                        if (-1.0..=24.0).contains(&da) && da < ad {
                            ad = da;
                            above = Some((si, ci, bi));
                        }
                    }
                    if (-1.0..=100.0).contains(&db) && db < fd {
                        fd = db;
                        far = Some((si, ci, bi));
                    }
                }
            }
        }
        let host = below.or(above).or(far);
        if let Some((si, ci, bi)) = host {
            let col = &mut sections[si].columns[ci];
            let PageBlock::Text(t) = &mut col.blocks[bi] else {
                unreachable!()
            };
            if t.border.is_none() {
                let side = if below.is_none() && above.is_some() {
                    "bottom"
                } else {
                    "top"
                };
                let edge = if side == "top" {
                    t.bounds.y1
                } else {
                    t.bounds.y0
                };
                let inset = col.bounds.x1 - b.x1;
                let right = (b.x1 >= t.bounds.x1 - 2.0 && inset > 1.0).then_some(inset);
                t.border = Some(border(s, side, (cy - edge).abs(), None, right));
                continue;
            }
        }
        standalone.push(s);
    }
    standalone.sort_by(|a, b| b.bounds.width().total_cmp(&a.bounds.width()));
    let mut placed = 0;
    for s in &standalone {
        if placed >= 2 {
            break;
        }
        let b = s.bounds;
        let (cx, cy) = (b.center_x(), b.center_y());
        let mut best = None;
        let mut dy = f64::INFINITY;
        for (si, sec) in sections.iter().enumerate() {
            for (ci, col) in sec.columns.iter().enumerate() {
                let c = col.bounds;
                if cx < c.x0 - 2.0 || cx > c.x1 + 2.0 {
                    continue;
                }
                let d = if cy > c.y1 {
                    cy - c.y1
                } else if cy < c.y0 {
                    c.y0 - cy
                } else {
                    0.0
                };
                if d < dy {
                    dy = d;
                    best = Some((si, ci));
                }
            }
        }
        let Some((si, ci)) = best else { continue };
        let col = &mut sections[si].columns[ci];
        if col
            .blocks
            .iter()
            .any(|v| !floating(v) && bounds(v).y0 < b.y1 + 1.0 && bounds(v).y1 > b.y0 - 1.0)
        {
            continue;
        }
        if si > 0
            && !col
                .blocks
                .iter()
                .any(|v| !floating(v) && bounds(v).y1 >= b.y1)
        {
            continue;
        }
        placed += 1;
        let t = TextBlock {
            lines: Vec::new(),
            bounds: b,
            align: Alignment::Left,
            first_line_indent_pt: 0.0,
            direction: Direction::Ltr,
            list: None,
            stitched_from_line: None,
            toc_entry: None,
            spacing_before_pt: None,
            border: Some(border(
                s,
                "top",
                0.0,
                Some((b.x0 - col.bounds.x0).max(0.0)),
                Some((col.bounds.x1 - b.x1).max(0.0)),
            )),
            card_id: None,
        };
        let at = col
            .blocks
            .iter()
            .position(|v| bounds(v).y1 < b.y1)
            .unwrap_or(col.blocks.len());
        col.blocks.insert(at, PageBlock::Text(Box::new(t)));
        col.bounds = col.bounds.union(b);
        sections[si].bounds = sections[si].bounds.union(b);
    }
    DecorResult {
        ignored_vertical: ignored,
        dropped_bars: standalone.len() - placed,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn excluded_and_consumed_rules_do_not_count() {
        let s = Stroke {
            bounds: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 50.0,
            },
            orientation: "v".into(),
            width_pt: 1.0,
            color: "000000".into(),
            from_form: false,
        };
        let result = apply_decor_borders(&mut [], &[s], &[], &HashSet::from([0]));
        assert_eq!(result.ignored_vertical, 0);
        assert_eq!(result.dropped_bars, 0);
    }
}
