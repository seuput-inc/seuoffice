//! Normalization and background selection for PDF paths.
use super::super::{
    geometry::{Rect, approx_eq, bbox_of_points},
    ir::{Fill, PageShapes, PdfChar, RawPath, RawSubpath, Stroke},
};

pub(super) fn rect_of_subpath(sub: &RawSubpath) -> Option<Rect> {
    let mut p: Vec<(f64, f64)> = Vec::new();
    for &v in &sub.points {
        if p.last()
            .is_none_or(|q| !approx_eq(v.0, q.0, 0.5) || !approx_eq(v.1, q.1, 0.5))
        {
            p.push(v);
        }
    }
    if p.len() >= 2
        && approx_eq(p[0].0, p[p.len() - 1].0, 0.5)
        && approx_eq(p[0].1, p[p.len() - 1].1, 0.5)
    {
        p.pop();
    }
    let mut i = 0;
    while p.len() > 4 && i < p.len() {
        let a = p[(i + p.len() - 1) % p.len()];
        let b = p[i];
        let c = p[(i + 1) % p.len()];
        if (approx_eq(a.0, b.0, 0.5) && approx_eq(b.0, c.0, 0.5))
            || (approx_eq(a.1, b.1, 0.5) && approx_eq(b.1, c.1, 0.5))
        {
            p.remove(i);
            i = 0;
        } else {
            i += 1;
        }
    }
    if p.len() != 4 {
        return None;
    }
    for i in 0..4 {
        let a = p[i];
        let b = p[(i + 1) % 4];
        let c = p[(i + 2) % 4];
        let h = approx_eq(a.1, b.1, 0.5);
        let v = approx_eq(a.0, b.0, 0.5);
        if h == v || h == approx_eq(b.1, c.1, 0.5) {
            return None;
        }
    }
    Some(bbox_of_points(&p))
}
fn stroke(h: bool, a: f64, b: f64, pos: f64, width: f64, color: &str) -> Stroke {
    let w = width.max(0.1);
    Stroke {
        bounds: if h {
            Rect {
                x0: a.min(b),
                x1: a.max(b),
                y0: pos - w / 2.0,
                y1: pos + w / 2.0,
            }
        } else {
            Rect {
                x0: pos - w / 2.0,
                x1: pos + w / 2.0,
                y0: a.min(b),
                y1: a.max(b),
            }
        },
        orientation: if h { "h" } else { "v" }.into(),
        width_pt: w,
        color: color.into(),
        from_form: false,
    }
}
fn clip(r: Rect, c: Option<Rect>) -> Option<Rect> {
    if let Some(c) = c {
        let b = Rect {
            x0: r.x0.max(c.x0),
            y0: r.y0.max(c.y0),
            x1: r.x1.min(c.x1),
            y1: r.y1.min(c.y1),
        };
        (b.x1 > b.x0 && b.y1 > b.y0).then_some(b)
    } else {
        Some(r)
    }
}
fn thin(r: Rect, color: &str) -> Option<Stroke> {
    let w = r.width();
    let h = r.height();
    let mn = w.min(h);
    if mn > 3.2 || w.max(h) < 3.0 * mn.max(0.1) {
        return None;
    }
    Some(if w >= h {
        stroke(true, r.x0, r.x1, r.center_y(), h, color)
    } else {
        stroke(false, r.y0, r.y1, r.center_x(), w, color)
    })
}
fn edges(r: Rect, p: &RawPath, rounded: bool) -> Vec<Stroke> {
    let mut out = Vec::new();
    let c = p.clip_box;
    let ys = if rounded { [r.y0, r.y1] } else { [r.y1, r.y0] };
    for y in ys {
        if c.is_some_and(|c| y < c.y0 - 0.5 || y > c.y1 + 0.5) {
            continue;
        }
        let x0 = c.map_or(r.x0, |c| r.x0.max(c.x0));
        let x1 = c.map_or(r.x1, |c| r.x1.min(c.x1));
        if x1 > x0 {
            out.push(stroke(true, x0, x1, y, p.stroke_width, &p.stroke_color));
        }
    }
    for x in [r.x0, r.x1] {
        if c.is_some_and(|c| x < c.x0 - 0.5 || x > c.x1 + 0.5) {
            continue;
        }
        let y0 = c.map_or(r.y0, |c| r.y0.max(c.y0));
        let y1 = c.map_or(r.y1, |c| r.y1.min(c.y1));
        if y1 > y0 {
            out.push(stroke(false, y0, y1, x, p.stroke_width, &p.stroke_color));
        }
    }
    out
}
fn rounded_edges(s: &RawSubpath, p: &RawPath) -> Option<Vec<Stroke>> {
    let lines = s.line_to.as_ref()?;
    if s.points.len() < 8 {
        return None;
    }
    let b = bbox_of_points(&s.points);
    if b.width() < 8.0 || b.height() < 8.0 {
        return None;
    }
    let (mut top, mut bottom, mut left, mut right) = (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64);
    for i in 1..s.points.len() {
        if !lines.get(i).copied().unwrap_or(false) {
            continue;
        }
        let a = s.points[i - 1];
        let z = s.points[i];
        if approx_eq(a.1, z.1, 0.5) {
            let len = (z.0 - a.0).abs();
            let y = (a.1 + z.1) / 2.0;
            if y - b.y0 <= 1.0 {
                top = top.max(len)
            } else if b.y1 - y <= 1.0 {
                bottom = bottom.max(len)
            } else {
                return None;
            }
        } else if approx_eq(a.0, z.0, 0.5) {
            let len = (z.1 - a.1).abs();
            let x = (a.0 + z.0) / 2.0;
            if x - b.x0 <= 1.0 {
                left = left.max(len)
            } else if b.x1 - x <= 1.0 {
                right = right.max(len)
            } else {
                return None;
            }
        } else {
            return None;
        }
    }
    if top < b.width() * 0.5
        || bottom < b.width() * 0.5
        || left < b.height() * 0.5
        || right < b.height() * 0.5
    {
        return None;
    }
    let e = edges(b, p, true);
    if e.is_empty() { None } else { Some(e) }
}
fn polygon_area(s: &RawSubpath) -> f64 {
    if s.points.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..s.points.len() {
        let a = s.points[i];
        let b = s.points[(i + 1) % s.points.len()];
        sum += a.0 * b.1 - b.0 * a.1;
    }
    sum.abs() / 2.0
}
fn fill(r: Rect, p: &RawPath, seq: usize) -> Fill {
    Fill {
        bounds: r,
        color: p.fill_color.clone(),
        alpha: p.fill_alpha.filter(|&a| a < 255),
        z: p.z,
        sequence: Some(seq),
        geometry: None,
        corner_radius_pt: None,
    }
}
fn curved(s: &RawSubpath, b: Rect, f: &mut Fill) {
    let (mut straight, mut mh, mut mv) = (0.0_f64, 0.0_f64, 0.0_f64);
    for i in 1..s.points.len() {
        if !s
            .line_to
            .as_ref()
            .and_then(|v| v.get(i))
            .copied()
            .unwrap_or(false)
        {
            continue;
        }
        let a = s.points[i - 1];
        let z = s.points[i];
        let dx = (z.0 - a.0).abs();
        let dy = (z.1 - a.1).abs();
        straight += dx.hypot(dy);
        if approx_eq(a.1, z.1, 0.5) {
            mh = mh.max(dx)
        } else if approx_eq(a.0, z.0, 0.5) {
            mv = mv.max(dy)
        }
    }
    if straight < 0.2 * 2.0 * (b.width() + b.height()) {
        f.geometry = Some("ellipse".into());
    } else {
        f.geometry = Some("roundRect".into());
        f.corner_radius_pt = Some(
            (b.width() / 2.0)
                .min(b.height() / 2.0)
                .min((b.width() - mh) / 2.0)
                .min((b.height() - mv) / 2.0)
                .max(0.0),
        );
    }
}
pub(super) fn normalize_shapes(paths: &[RawPath], rounded_rect_edges: bool) -> PageShapes {
    let mut out = PageShapes {
        strokes: Vec::new(),
        fills: Vec::new(),
        ignored_paths: 0,
        curved_fills: Vec::new(),
    };
    for (seq, p) in paths.iter().enumerate() {
        let start = out.strokes.len();
        if !p.filled && !p.stroked {
            continue;
        }
        let filled = p.filled && p.fill_alpha.unwrap_or(255) >= 128;
        if !filled && !p.stroked {
            out.ignored_paths += 1;
            continue;
        }
        for s in &p.subpaths {
            if s.has_curves {
                if filled
                    && !s.points.is_empty()
                    && let Some(b) = clip(bbox_of_points(&s.points), p.clip_box)
                {
                    let area = b.width() * b.height();
                    if area > 0.0 && polygon_area(s).min(area) >= 0.55 * area {
                        let mut f = fill(b, p, seq);
                        curved(s, b, &mut f);
                        out.curved_fills.push(f);
                    }
                }
                if rounded_rect_edges
                    && p.stroked
                    && let Some(e) = rounded_edges(s, p)
                {
                    out.strokes.extend(e);
                    continue;
                }
                out.ignored_paths += 1;
                continue;
            }
            let raw = rect_of_subpath(s);
            let r = raw.and_then(|r| clip(r, p.clip_box));
            if raw.is_some() && r.is_none() {
                continue;
            }
            if let Some(r) = r {
                if let Some(t) = thin(
                    r,
                    if filled {
                        &p.fill_color
                    } else {
                        &p.stroke_color
                    },
                ) {
                    out.strokes.push(t);
                    continue;
                }
                if filled {
                    out.fills.push(fill(r, p, seq));
                }
                if p.stroked {
                    out.strokes.extend(edges(raw.unwrap(), p, false));
                }
                continue;
            }
            if filled {
                let axis = s
                    .points
                    .iter()
                    .copied()
                    .zip(s.points.iter().copied().cycle().skip(1))
                    .take(if s.points.len() > 2 {
                        s.points.len()
                    } else {
                        s.points.len().saturating_sub(1)
                    })
                    .all(|(a, b)| approx_eq(a.0, b.0, 0.5) || approx_eq(a.1, b.1, 0.5));
                let bar = if axis {
                    clip(bbox_of_points(&s.points), p.clip_box).and_then(|r| thin(r, &p.fill_color))
                } else {
                    None
                };
                if let Some(t) = bar {
                    out.strokes.push(t);
                    continue;
                }
                out.ignored_paths += 1;
            }
            if !p.stroked {
                continue;
            }
            let n = s.points.len();
            let mut diagonal = false;
            for i in 1..n + usize::from(s.closed && n > 2) {
                let a = s.points[i - 1];
                let b = s.points[i % n];
                if approx_eq(a.1, b.1, 0.5) {
                    if approx_eq(a.0, b.0, 0.5) {
                        continue;
                    }
                    let y = (a.1 + b.1) / 2.0;
                    if p.clip_box.is_some_and(|c| y < c.y0 - 0.5 || y > c.y1 + 0.5) {
                        continue;
                    }
                    let x0 = p.clip_box.map_or(a.0.min(b.0), |c| a.0.min(b.0).max(c.x0));
                    let x1 = p.clip_box.map_or(a.0.max(b.0), |c| a.0.max(b.0).min(c.x1));
                    if x1 > x0 {
                        out.strokes
                            .push(stroke(true, x0, x1, y, p.stroke_width, &p.stroke_color));
                    }
                } else if approx_eq(a.0, b.0, 0.5) {
                    let x = (a.0 + b.0) / 2.0;
                    if p.clip_box.is_some_and(|c| x < c.x0 - 0.5 || x > c.x1 + 0.5) {
                        continue;
                    }
                    let y0 = p.clip_box.map_or(a.1.min(b.1), |c| a.1.min(b.1).max(c.y0));
                    let y1 = p.clip_box.map_or(a.1.max(b.1), |c| a.1.max(b.1).min(c.y1));
                    if y1 > y0 {
                        out.strokes
                            .push(stroke(false, y0, y1, x, p.stroke_width, &p.stroke_color));
                    }
                } else {
                    diagonal = true
                }
            }
            if diagonal {
                out.ignored_paths += 1;
            }
        }
        if p.from_form {
            for s in &mut out.strokes[start..] {
                s.from_form = true;
            }
        }
    }
    out
}

fn rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let v = hex.trim().as_bytes();
    if v.len() != 6 || !v.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let n = |i: usize| u8::from_str_radix(std::str::from_utf8(&v[i..i + 2]).ok()?, 16).ok();
    Some((n(0)?, n(2)?, n(4)?))
}
pub(super) fn is_near_white(hex: &str) -> bool {
    rgb(hex).is_some_and(|(r, g, b)| r >= 247 && g >= 247 && b >= 247)
}
pub(super) fn hex_luminance(hex: &str) -> f64 {
    rgb(hex).map_or(0.0, |(r, g, b)| {
        (0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64) / 255.0
    })
}
fn inside(r: Rect, b: Rect) -> bool {
    r.center_x() >= b.x0 && r.center_x() <= b.x1 && r.center_y() >= b.y0 && r.center_y() <= b.y1
}
pub(super) fn extract_full_bleed_tiles(
    shapes: &mut PageShapes,
    char_count: usize,
    width: f64,
    height: f64,
) -> Vec<Fill> {
    if char_count > 10 {
        return Vec::new();
    }
    let indices: Vec<_> = shapes
        .fills
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            f.alpha.is_none_or(|a| a >= 250)
                && f.bounds.width() * f.bounds.height() >= 0.08 * width * height
        })
        .map(|(i, _)| i)
        .collect();
    if indices.len() < 2 || indices.len() > 6 {
        return Vec::new();
    }
    let mut covered = 0;
    for y in 0..24 {
        for x in 0..24 {
            let cx = (x as f64 + 0.5) / 24.0 * width;
            let cy = (y as f64 + 0.5) / 24.0 * height;
            if indices.iter().any(|&i| {
                let b = shapes.fills[i].bounds;
                cx >= b.x0 && cx <= b.x1 && cy >= b.y0 && cy <= b.y1
            }) {
                covered += 1;
            }
        }
    }
    if (covered as f64) / 576.0 < 0.95 {
        return Vec::new();
    }
    let mut tiles = Vec::new();
    let mut i = 0;
    shapes.fills.retain(|f| {
        let take = indices.binary_search(&i).is_ok();
        i += 1;
        if take {
            tiles.push(f.clone());
        }
        !take
    });
    shapes.strokes.retain(|s| {
        !tiles.iter().any(|f| {
            s.bounds.x0 >= f.bounds.x0 - 2.0
                && s.bounds.x1 <= f.bounds.x1 + 2.0
                && s.bounds.y0 >= f.bounds.y0 - 2.0
                && s.bounds.y1 <= f.bounds.y1 + 2.0
        })
    });
    for f in &mut tiles {
        f.bounds.x0 = f.bounds.x0.max(0.0);
        f.bounds.y0 = f.bounds.y0.max(0.0);
        f.bounds.x1 = f.bounds.x1.min(width);
        f.bounds.y1 = f.bounds.y1.min(height);
    }
    tiles
}
pub(super) fn extract_page_background(
    fills: &mut Vec<Fill>,
    width: f64,
    height: f64,
    content: Option<Rect>,
) -> Option<String> {
    let page = Rect {
        x0: 0.0,
        y0: 0.0,
        x1: width,
        y1: height,
    };
    let mut color = None;
    for i in (0..fills.len()).rev() {
        let b = fills[i].bounds;
        if b.covers(page, 0.94) || content.is_some_and(|c| b.covers(c, 0.94)) {
            if color.is_none() {
                color = Some(fills[i].color.clone());
            }
            fills.remove(i);
        }
    }
    color
}
pub(super) fn extract_background_panels(
    fills: &mut Vec<Fill>,
    chars: &[Rect],
    width: f64,
    height: f64,
    content: Option<Rect>,
) -> Vec<Fill> {
    let area = width * height;
    if area <= 0.0 {
        return Vec::new();
    }
    let edge = content.unwrap_or(Rect {
        x0: 0.0,
        y0: 0.0,
        x1: width,
        y1: height,
    });
    let mut panels = Vec::new();
    while !fills.is_empty() {
        let f = &fills[0];
        let b = f.bounds;
        let share = b.width() * b.height() / area;
        let full_h =
            b.height() >= edge.height() * 0.96 && (b.x0 <= edge.x0 + 2.0 || b.x1 >= edge.x1 - 2.0);
        let full_w =
            b.width() >= edge.width() * 0.96 && (b.y0 <= edge.y0 + 2.0 || b.y1 >= edge.y1 - 2.0);
        if is_near_white(&f.color)
            || !(0.18..=0.85).contains(&share)
            || !(full_h || full_w)
            || chars.iter().filter(|&&c| inside(c, b)).take(4).count() < 4
        {
            break;
        }
        panels.push(fills.remove(0));
    }
    panels
}
fn qualifies(
    fill: &Fill,
    rounded: bool,
    chars: &[PdfChar],
    grids: &[Rect],
    page_area: f64,
    pool: &[Fill],
    has_backdrop: bool,
) -> bool {
    let b = fill.bounds;
    let area = b.width() * b.height();
    let pool_eligible = !is_near_white(&fill.color) && area >= 600.0;
    let capsule = rounded && (has_backdrop || !is_near_white(&fill.color));
    if (!pool_eligible && !capsule)
        || area <= 0.0
        || area > 0.85 * page_area
        || grids.iter().any(|&g| inside(b, g))
    {
        return false;
    }
    let luma = hex_luminance(&fill.color);
    let mut lumas = Vec::new();
    let mut heights = Vec::new();
    let (mut x0, mut x1) = (f64::INFINITY, f64::NEG_INFINITY);
    for c in chars {
        if c.code <= 0x20 || !inside(c.bounds, b) {
            continue;
        }
        lumas.push(hex_luminance(&c.color));
        heights.push(c.bounds.height());
        x0 = x0.min(c.bounds.x0);
        x1 = x1.max(c.bounds.x1);
    }
    let total = lumas.len();
    let light = lumas.iter().filter(|&&v| v >= 0.85).count();
    lumas.sort_by(f64::total_cmp);
    heights.sort_by(f64::total_cmp);
    if pool_eligible {
        if luma <= 0.75 && light >= 2 && light as f64 / (total.max(1) as f64) >= 0.6 {
            return true;
        }
        if area >= 0.1 * page_area && total >= 6 && (lumas[total / 2] - luma).abs() >= 0.3 {
            return true;
        }
        if luma <= 0.75
            && area >= 0.1 * page_area
            && pool.iter().any(|o| {
                let r = o.bounds;
                r.area() >= 600.0
                    && r.area() <= 0.85 * area
                    && r.x0 >= b.x0 - 1.0
                    && r.x1 <= b.x1 + 1.0
                    && r.y0 >= b.y0 - 1.0
                    && r.y1 <= b.y1 + 1.0
                    && hex_luminance(&o.color) - luma >= 0.3
            })
        {
            return true;
        }
        if area >= 0.02 * page_area
            && total >= 2
            && b.height() >= 2.5 * heights[total / 2]
            && (lumas[total / 2] - luma).abs() >= 0.3
        {
            return true;
        }
    }
    capsule
        && total >= 2
        && b.width() >= 2.0 * b.height()
        && x1 - x0 >= 0.3 * b.width()
        && (lumas[total / 2] - luma).abs() >= 0.3
}
pub(super) fn extract_text_backdrops(
    fills: &mut Vec<Fill>,
    chars: &[PdfChar],
    grids: &[Rect],
    width: f64,
    height: f64,
    curved_fills: &[Fill],
    has_page_backdrop: bool,
) -> Vec<Fill> {
    let area = width * height;
    if area <= 0.0 {
        return Vec::new();
    }
    let pool: Vec<_> = fills.iter().chain(curved_fills).cloned().collect();
    let mut out = Vec::new();
    for i in (0..fills.len()).rev() {
        if qualifies(
            &fills[i],
            false,
            chars,
            grids,
            area,
            &pool,
            has_page_backdrop,
        ) {
            out.push(fills.remove(i));
        }
    }
    out.reverse();
    for (i, f) in curved_fills.iter().enumerate() {
        let group: Vec<_> = curved_fills
            .iter()
            .enumerate()
            .filter(|(_, o)| o.z == f.z)
            .collect();
        let mut overlaps = 0;
        if group.len() >= 3 {
            for a in 0..group.len() {
                for b in a + 1..group.len() {
                    let x = group[a].1.bounds;
                    let y = group[b].1.bounds;
                    let smaller = x.area().min(y.area());
                    if smaller > 0.0 && x.intersection_area(y) >= 0.2 * smaller {
                        overlaps += 1;
                        break;
                    }
                }
                if overlaps >= 3 {
                    break;
                }
            }
        }
        let _ = i;
        if overlaps + 1 >= 3 {
            continue;
        }
        if qualifies(f, true, chars, grids, area, &pool, has_page_backdrop) {
            out.push(f.clone());
        }
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rectangle_and_white_detection() {
        let s = RawSubpath {
            points: vec![
                (0.0, 0.0),
                (5.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ],
            closed: true,
            has_curves: false,
            line_to: None,
        };
        assert_eq!(
            rect_of_subpath(&s),
            Some(Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 10.0,
                y1: 10.0
            })
        );
        assert!(is_near_white("F7FFFF"));
        assert!(!is_near_white("F6FFFF"));
    }
    #[test]
    fn clipped_fill_and_authored_edges_are_preserved() {
        let path = RawPath {
            subpaths: vec![RawSubpath {
                points: vec![(0.0, 0.0), (100.0, 0.0), (100.0, 50.0), (0.0, 50.0)],
                closed: true,
                has_curves: false,
                line_to: None,
            }],
            filled: true,
            stroked: true,
            fill_color: "112233".into(),
            stroke_color: "000000".into(),
            stroke_width: 1.0,
            fill_alpha: Some(255),
            from_form: true,
            z: Some(7),
            clip_box: Some(Rect {
                x0: 10.0,
                y0: 0.0,
                x1: 90.0,
                y1: 50.0,
            }),
        };
        let shapes = normalize_shapes(&[path], false);
        assert_eq!(
            shapes.fills[0].bounds,
            Rect {
                x0: 10.0,
                y0: 0.0,
                x1: 90.0,
                y1: 50.0
            }
        );
        assert_eq!(shapes.strokes.len(), 2);
        assert!(
            shapes
                .strokes
                .iter()
                .all(|s| s.from_form && s.orientation == "h")
        );
    }
}
