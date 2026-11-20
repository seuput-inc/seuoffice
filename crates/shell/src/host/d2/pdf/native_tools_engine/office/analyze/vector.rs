//! Vector-art detection and paragraph vetoes in PDF page coordinates.
use super::super::{
    geometry::{Rect, approx_eq, bbox_of_points, median},
    ir::{PdfChar, RawPath, RawSubpath},
};
use super::shapes::rect_of_subpath;

fn inside(r: Rect, b: Rect) -> bool {
    r.center_x() >= b.x0 && r.center_x() <= b.x1 && r.center_y() >= b.y0 && r.center_y() <= b.y1
}
fn touches(a: Rect, b: Rect, t: f64) -> bool {
    a.x0 <= b.x1 + t && b.x0 <= a.x1 + t && a.y0 <= b.y1 + t && b.y0 <= a.y1 + t
}
fn art(path: &RawPath, sub: &RawSubpath) -> bool {
    if sub.has_curves {
        return true;
    }
    if rect_of_subpath(sub).is_some() {
        return false;
    }
    if path.filled {
        return true;
    }
    let diagonal =
        |a: (f64, f64), b: (f64, f64)| !approx_eq(a.0, b.0, 0.5) && !approx_eq(a.1, b.1, 0.5);
    sub.points.windows(2).any(|p| diagonal(p[0], p[1]))
        || (sub.closed
            && sub.points.len() > 2
            && diagonal(*sub.points.last().unwrap(), sub.points[0]))
}
fn root(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}
fn clusters(boxes: &[Rect], tol: f64) -> Vec<Vec<Rect>> {
    let mut parent: Vec<usize> = (0..boxes.len()).collect();
    for i in 0..boxes.len() {
        for j in i + 1..boxes.len() {
            if touches(boxes[i], boxes[j], tol) {
                let a = root(&mut parent, i);
                let b = root(&mut parent, j);
                if a != b {
                    parent[b] = a;
                }
            }
        }
    }
    let mut groups: Vec<(usize, Vec<Rect>)> = Vec::new();
    for (i, &b) in boxes.iter().enumerate() {
        let r = root(&mut parent, i);
        if let Some((_, v)) = groups.iter_mut().find(|(k, _)| *k == r) {
            v.push(b);
        } else {
            groups.push((r, vec![b]));
        }
    }
    groups.into_iter().map(|(_, v)| v).collect()
}
fn admit(region: Rect, chars: &[&PdfChar], body: f64) -> bool {
    if region.width() < 24.0 || region.height() < 24.0 || region.area() <= 0.0 {
        return false;
    }
    let mut ink = 0.0;
    let mut count = 0;
    let mut rows: Vec<(f64, usize, f64, f64)> = Vec::new();
    for c in chars {
        if !inside(c.bounds, region) {
            continue;
        }
        count += 1;
        ink += c.bounds.area();
        if c.font_size < (0.75 * body).max(6.5) {
            continue;
        }
        // JavaScript Math.round, including negative half values.
        let key = (c.origin_y / (body / 2.0).max(1.0) + 0.5).floor();
        if let Some(row) = rows.iter_mut().find(|r| r.0 == key) {
            row.1 += 1;
            row.2 = row.2.min(c.bounds.x0);
            row.3 = row.3.max(c.bounds.x1);
        } else {
            rows.push((key, 1, c.bounds.x0, c.bounds.x1));
        }
    }
    !(count > 2 && ink / region.area() > 0.15)
        && rows
            .iter()
            .filter(|r| r.1 >= 6 && r.3 - r.2 >= 60.0)
            .count()
            < 3
}
pub(in super::super) fn detect_vector_regions(
    paths: &[RawPath],
    chars: &[PdfChar],
    page_rect: Rect,
) -> Vec<Rect> {
    let mut art_boxes = Vec::new();
    let mut all = Vec::new();
    for path in paths {
        if !path.filled && !path.stroked {
            continue;
        }
        for sub in &path.subpaths {
            if sub.points.is_empty() {
                continue;
            }
            let b = bbox_of_points(&sub.points);
            all.push(b);
            if art(path, sub) {
                art_boxes.push(b);
            }
        }
    }
    if art_boxes.len() < 6 {
        return Vec::new();
    }
    let visible: Vec<_> = chars
        .iter()
        .filter(|c| !c.is_generated && c.code > 0x20)
        .collect();
    let sizes: Vec<_> = visible.iter().map(|c| c.font_size).collect();
    let body = median(&sizes);
    let body = if body == 0.0 { 12.0 } else { body };
    let mut regions = Vec::new();
    for group in clusters(&art_boxes, 10.0) {
        if group.len() < 6 {
            continue;
        }
        let base = group.into_iter().reduce(Rect::union).unwrap();
        let mut region = base;
        for _ in 0..2 {
            for &b in &all {
                if inside(b, region) {
                    region = region.union(b);
                }
            }
        }
        let chosen = if admit(region, &visible, body) {
            region
        } else if admit(base, &visible, body) {
            base
        } else {
            continue;
        };
        regions.push(Rect {
            x0: page_rect.x0.max(chosen.x0 - 2.0),
            y0: page_rect.y0.max(chosen.y0 - 2.0),
            x1: page_rect.x1.min(chosen.x1 + 2.0),
            y1: page_rect.y1.min(chosen.y1 + 2.0),
        });
    }
    clusters(&regions, 0.0)
        .into_iter()
        .map(|g| g.into_iter().reduce(Rect::union).unwrap())
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proximity_groups_are_transitive_and_ordered() {
        let r = |x| Rect {
            x0: x,
            y0: 0.0,
            x1: x + 5.0,
            y1: 5.0,
        };
        let g = clusters(&[r(0.0), r(14.0), r(28.0), r(100.0)], 10.0);
        assert_eq!(g.iter().map(Vec::len).collect::<Vec<_>>(), vec![3, 1]);
    }
    #[test]
    fn curved_cluster_is_padded_and_clipped() {
        let subpaths = (0..6)
            .map(|i| RawSubpath {
                points: vec![(i as f64 * 5.0, 0.0), (i as f64 * 5.0 + 10.0, 30.0)],
                closed: false,
                has_curves: true,
                line_to: None,
            })
            .collect();
        let path = RawPath {
            subpaths,
            filled: true,
            stroked: false,
            fill_color: "000000".into(),
            stroke_color: "000000".into(),
            stroke_width: 1.0,
            fill_alpha: None,
            from_form: false,
            z: None,
            clip_box: None,
        };
        let regions = detect_vector_regions(
            &[path],
            &[],
            Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 100.0,
                y1: 100.0,
            },
        );
        assert_eq!(
            regions,
            vec![Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 37.0,
                y1: 32.0
            }]
        );
    }
}
