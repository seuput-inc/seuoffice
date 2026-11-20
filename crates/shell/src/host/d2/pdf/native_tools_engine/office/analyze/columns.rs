//! XY-cut section and column detection in page coordinates.
use super::super::{
    geometry::{
        Interval, Rect, complement_intervals, intersect_intervals, median, merge_intervals,
    },
    ir::{Direction, PageBlock},
    script::{self, Script},
};
use super::units::LineUnit;
use std::collections::HashSet;
#[derive(Debug, Clone, Copy)]
pub(super) struct SectionElement<'a> {
    pub bounds: Rect,
    pub unit: Option<&'a LineUnit>,
    pub block: Option<&'a PageBlock>,
}
#[derive(Debug, Clone)]
pub(super) struct LayoutColumn<'a> {
    pub bounds: Rect,
    pub elements: Vec<SectionElement<'a>>,
}
#[derive(Debug, Clone)]
pub(super) struct LayoutSection<'a> {
    pub bounds: Rect,
    pub columns: Vec<LayoutColumn<'a>>,
    pub gutters: Vec<Interval>,
    pub direction: Direction,
}
#[derive(Clone, Copy)]
struct Item<'a> {
    index: usize,
    value: SectionElement<'a>,
}
struct Slab {
    top: f64,
    bottom: f64,
    cover: Vec<Interval>,
    gaps: Vec<Interval>,
}
fn internal(gaps: &[Interval], cover: &[Interval]) -> Vec<Interval> {
    gaps.iter()
        .copied()
        .filter(|g| {
            cover.iter().any(|c| c.hi <= g.lo + 0.01) && cover.iter().any(|c| c.lo >= g.hi - 0.01)
        })
        .collect()
}
fn union(items: &[Item<'_>]) -> Rect {
    items
        .iter()
        .map(|e| e.value.bounds)
        .reduce(Rect::union)
        .unwrap_or_default()
}
fn slabs(items: &[Item<'_>], left: f64, right: f64) -> Vec<Slab> {
    let mut ys: Vec<_> = items
        .iter()
        .flat_map(|e| [e.value.bounds.y0, e.value.bounds.y1])
        .collect();
    ys.sort_by(|a, b| b.total_cmp(a));
    ys.dedup_by(|a, b| *a == *b);
    let mut out = Vec::new();
    for y in ys.windows(2) {
        let (top, bottom) = (y[0], y[1]);
        let cover: Vec<_> = items
            .iter()
            .filter(|e| e.value.bounds.y1 > bottom && e.value.bounds.y0 < top)
            .map(|e| Interval {
                lo: e.value.bounds.x0,
                hi: e.value.bounds.x1,
            })
            .collect();
        if cover.is_empty() {
            continue;
        }
        let cover = merge_intervals(&cover, 0.);
        let gaps = complement_intervals(&cover, left, right);
        out.push(Slab {
            top,
            bottom,
            cover,
            gaps,
        });
    }
    out
}
fn wide(gaps: &[Interval], minimum: f64) -> Vec<Interval> {
    gaps.iter()
        .copied()
        .filter(|g| g.hi - g.lo >= minimum)
        .collect()
}
fn sweep(slabs: Vec<Slab>, minimum: f64, min_height: f64) -> Vec<(f64, f64)> {
    let mut ranges = Vec::new();
    let mut open: Option<(f64, f64, Vec<Interval>, Vec<Interval>)> = None;
    for slab in slabs {
        let Some((top, bottom, gaps, cover)) = open.take() else {
            open = Some((slab.top, slab.bottom, slab.gaps, slab.cover));
            continue;
        };
        let candidate = wide(&intersect_intervals(&gaps, &slab.gaps), minimum);
        let combined = merge_intervals(&[cover.as_slice(), slab.cover.as_slice()].concat(), 0.);
        let trial = internal(&candidate, &combined);
        let old = internal(&wide(&gaps, minimum), &cover);
        let next = internal(&wide(&slab.gaps, minimum), &slab.cover);
        let fresh =
            !trial.is_empty() && internal(&trial, &cover).is_empty() && top - bottom >= min_height;
        let killed = old
            .iter()
            .any(|g| !trial.iter().any(|t| t.lo < g.hi && t.hi > g.lo));
        if !fresh && !killed && (!trial.is_empty() || (old.is_empty() && next.is_empty())) {
            open = Some((
                top,
                slab.bottom,
                intersect_intervals(&gaps, &slab.gaps),
                combined,
            ));
        } else {
            ranges.push((top, bottom));
            open = Some((slab.top, slab.bottom, slab.gaps, slab.cover));
        }
    }
    if let Some((top, bottom, _, _)) = open {
        ranges.push((top, bottom))
    }
    ranges
}
fn direction(items: &[Item<'_>]) -> Direction {
    let (mut rtl, mut ltr) = (0, 0);
    for c in items
        .iter()
        .filter_map(|i| i.value.unit)
        .flat_map(|u| &u.chars)
    {
        if script::rtl(c.script) {
            rtl += 1
        } else if c.script != Script::Common {
            ltr += 1
        }
    }
    if rtl > ltr {
        Direction::Rtl
    } else {
        Direction::Ltr
    }
}
fn order(items: &mut [Item<'_>]) {
    items.sort_by(|a, b| {
        b.value
            .bounds
            .y1
            .total_cmp(&a.value.bounds.y1)
            .then_with(|| a.value.bounds.x0.total_cmp(&b.value.bounds.x0))
    })
}
fn single<'a>(mut items: Vec<Item<'a>>) -> LayoutSection<'a> {
    let bounds = union(&items);
    let direction = direction(&items);
    order(&mut items);
    LayoutSection {
        bounds,
        columns: vec![LayoutColumn {
            bounds,
            elements: items.into_iter().map(|e| e.value).collect(),
        }],
        gutters: Vec::new(),
        direction,
    }
}
fn split<'a>(items: &[Item<'a>], bounds: Rect, gutters: &[Interval]) -> Vec<Vec<Item<'a>>> {
    let mut cuts = Vec::with_capacity(gutters.len() + 2);
    cuts.push(bounds.x0);
    cuts.extend(gutters.iter().map(|g| (g.lo + g.hi) / 2.));
    cuts.push(bounds.x1);
    let mut result = vec![Vec::new(); cuts.len() - 1];
    for &item in items {
        let x = item.value.bounds.center_x();
        let index = cuts
            .windows(2)
            .position(|c| x >= c[0] && x <= c[1])
            .unwrap_or_else(|| if x < bounds.x0 { 0 } else { cuts.len() - 2 });
        result[index].push(item);
    }
    result
}
fn extent(items: &[Item<'_>]) -> (f64, f64) {
    if items.is_empty() {
        return (0., 0.);
    }
    let b = union(items);
    (b.width(), b.height())
}
fn build<'a>(items: Vec<Item<'a>>, minimum: f64) -> LayoutSection<'a> {
    let bounds = union(&items);
    let dir = direction(&items);
    let cover = merge_intervals(
        &items
            .iter()
            .map(|e| Interval {
                lo: e.value.bounds.x0,
                hi: e.value.bounds.x1,
            })
            .collect::<Vec<_>>(),
        0.,
    );
    let mut gaps: Option<Vec<Interval>> = None;
    for slab in slabs(&items, bounds.x0, bounds.x1) {
        gaps = Some(match gaps {
            None => slab.gaps,
            Some(g) => intersect_intervals(&g, &slab.gaps),
        });
    }
    let mut gutters = internal(&wide(&gaps.unwrap_or_default(), minimum), &cover);
    while !gutters.is_empty() {
        let mut columns = split(&items, bounds, &gutters);
        let tallest = columns.iter().map(|c| extent(c).1).fold(0., f64::max);
        let strong = |c: &Vec<Item<'a>>| {
            let (w, h) = extent(c);
            (c.len() >= 3
                || c.iter()
                    .any(|e| matches!(e.value.block, Some(PageBlock::Table(_)))))
                && w >= 24.
                && h >= 0.35 * tallest
        };
        let anchored = columns.iter().any(&strong);
        let weak = columns.iter().enumerate().find_map(|(i, c)| {
            if strong(c) {
                return None;
            }
            let flank_left = i.checked_sub(1).and_then(|j| gutters.get(j));
            let flank_right = gutters.get(i);
            let standalone = anchored
                && c.len() >= 2
                && extent(c).0 >= 24.
                && flank_left
                    .into_iter()
                    .chain(flank_right)
                    .all(|g| g.hi - g.lo >= 3. * minimum);
            (!standalone).then_some(i)
        });
        if let Some(weak) = weak {
            let left = weak.checked_sub(1).and_then(|i| gutters.get(i));
            let right = gutters.get(weak);
            let drop = match (left, right) {
                (None, _) => weak,
                (Some(_), None) => weak - 1,
                (Some(l), Some(r)) => {
                    if r.hi - r.lo >= l.hi - l.lo {
                        weak - 1
                    } else {
                        weak
                    }
                }
            };
            gutters.remove(drop);
            continue;
        }
        let result = columns
            .iter_mut()
            .map(|col| {
                order(col);
                let content = union(col);
                LayoutColumn {
                    bounds: Rect {
                        x0: content.x0,
                        x1: content.x1,
                        y0: bounds.y0,
                        y1: bounds.y1,
                    },
                    elements: col.iter().map(|e| e.value).collect(),
                }
            })
            .collect();
        return LayoutSection {
            bounds,
            columns: result,
            gutters,
            direction: dir,
        };
    }
    single(items)
}
fn peel<'a>(
    items: &[Item<'a>],
    height: f64,
    ratio: f64,
) -> (Vec<Item<'a>>, Vec<Item<'a>>, Vec<Item<'a>>) {
    let mut line = median(
        &items
            .iter()
            .map(|e| e.value.bounds.height())
            .collect::<Vec<_>>(),
    );
    if line == 0. {
        line = 12.
    }
    let gap = 1.5 * line;
    let top = height * (1. - ratio);
    let bottom = height * ratio;
    let mut head = Vec::new();
    let mut foot = Vec::new();
    let mut body = Vec::new();
    for &e in items {
        if e.value.bounds.y0 >= top {
            head.push(e)
        } else if e.value.bounds.y1 <= bottom {
            foot.push(e)
        } else {
            body.push(e)
        }
    }
    let head_ok = !head.is_empty()
        && !body.is_empty()
        && head
            .iter()
            .map(|e| e.value.bounds.y0)
            .fold(f64::INFINITY, f64::min)
            - body
                .iter()
                .map(|e| e.value.bounds.y1)
                .fold(f64::NEG_INFINITY, f64::max)
            >= gap;
    let foot_ok = !foot.is_empty()
        && !body.is_empty()
        && body
            .iter()
            .map(|e| e.value.bounds.y0)
            .fold(f64::INFINITY, f64::min)
            - foot
                .iter()
                .map(|e| e.value.bounds.y1)
                .fold(f64::NEG_INFINITY, f64::max)
            >= gap;
    if !head_ok && !foot_ok {
        return (Vec::new(), items.to_vec(), Vec::new());
    }
    if !head_ok {
        let mut combined = std::mem::take(&mut head);
        combined.extend(body);
        body = combined
    }
    if !foot_ok {
        body.extend(std::mem::take(&mut foot))
    }
    (head, body, foot)
}
pub(super) fn merge_twin_sections<'a>(sections: &[LayoutSection<'a>]) -> Vec<LayoutSection<'a>> {
    let mut out: Vec<LayoutSection<'a>> = Vec::new();
    for section in sections {
        let twins = out.last().is_some_and(|p| {
            p.columns.len() >= 2
                && p.direction == section.direction
                && p.columns.len() == section.columns.len()
                && p.gutters.len() == section.gutters.len()
                && p.gutters
                    .iter()
                    .zip(&section.gutters)
                    .all(|(a, b)| a.lo < b.hi && b.lo < a.hi)
        });
        if !twins {
            out.push(section.clone());
            continue;
        }
        if let Some(prev) = out.last_mut() {
            prev.bounds = prev.bounds.union(section.bounds);
            for (a, b) in prev.gutters.iter_mut().zip(&section.gutters) {
                a.lo = a.lo.max(b.lo);
                a.hi = a.hi.min(b.hi)
            }
            for (a, b) in prev.columns.iter_mut().zip(&section.columns) {
                a.bounds = a.bounds.union(b.bounds);
                a.elements.extend_from_slice(&b.elements)
            }
        }
    }
    out
}
pub(super) fn detect_sections<'a>(
    elements: &[SectionElement<'a>],
    height: f64,
    width: Option<f64>,
) -> Vec<LayoutSection<'a>> {
    if elements.is_empty() {
        return Vec::new();
    }
    let indexed: Vec<_> = elements
        .iter()
        .copied()
        .enumerate()
        .map(|(index, value)| Item { index, value })
        .collect();
    let landscape = width.is_some_and(|w| w > height);
    let (header, body, footer) = peel(&indexed, height, if landscape { 0.12 } else { 0.08 });
    let mut sections = Vec::new();
    if !header.is_empty() {
        sections.push(single(header))
    }
    if !body.is_empty() {
        let bounds = union(&body);
        let mut font = median(
            &body
                .iter()
                .filter_map(|e| e.value.unit.map(|u| u.font_size))
                .collect::<Vec<_>>(),
        );
        if font == 0. {
            font = 12.
        }
        let minimum = f64::max(if landscape { 24. } else { 9. }, 0.6 * font);
        let mut line = median(
            &body
                .iter()
                .map(|e| e.value.bounds.height())
                .collect::<Vec<_>>(),
        );
        if line == 0. {
            line = 12.
        }
        let mut placed = HashSet::new();
        for (top, bottom) in sweep(slabs(&body, bounds.x0, bounds.x1), minimum, 2.5 * line) {
            let in_range: Vec<_> = body
                .iter()
                .copied()
                .filter(|e| {
                    let y = e.value.bounds.center_y();
                    y <= top + 0.01 && y >= bottom - 0.01
                })
                .collect();
            if !in_range.is_empty() {
                placed.extend(in_range.iter().map(|e| e.index));
                sections.push(build(in_range, minimum));
            }
        }
        let stray: Vec<_> = body
            .into_iter()
            .filter(|e| !placed.contains(&e.index))
            .collect();
        if !stray.is_empty() {
            sections.push(single(stray))
        }
    }
    if !footer.is_empty() {
        sections.push(single(footer))
    }
    sections.sort_by(|a, b| b.bounds.y1.total_cmp(&a.bounds.y1));
    sections
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separated_blocks_form_two_columns() {
        let rects = [
            (0., 90.),
            (0., 70.),
            (0., 50.),
            (100., 90.),
            (100., 70.),
            (100., 50.),
        ];
        let elements: Vec<_> = rects
            .iter()
            .map(|(x, y)| SectionElement {
                bounds: Rect {
                    x0: *x,
                    x1: x + 50.,
                    y0: *y,
                    y1: y + 10.,
                },
                unit: None,
                block: None,
            })
            .collect();
        let sections = detect_sections(&elements, 120., Some(200.));
        assert!(sections.iter().any(|s| s.columns.len() == 2));
    }
}
