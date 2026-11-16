//! Lattice table geometry and reconstruction.
use super::super::{
    geometry::Rect,
    ir::{Fill, PageShapes, PdfChar, SoftEdges, Stroke, TableBlock, TableCell, TextBlock},
};
use std::collections::HashMap;

const CONNECT_TOL: f64 = 2.5;
const CELL: f64 = 24.0;

fn root(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}
fn touches(a: Rect, b: Rect) -> bool {
    a.x0 <= b.x1 + CONNECT_TOL
        && b.x0 <= a.x1 + CONNECT_TOL
        && a.y0 <= b.y1 + CONNECT_TOL
        && b.y0 <= a.y1 + CONNECT_TOL
}

/// Stroke indices in their original order, grouped by spatial contact.
pub(super) fn group_strokes(strokes: &[Stroke]) -> Vec<Vec<usize>> {
    let mut parent: Vec<_> = (0..strokes.len()).collect();
    let mut buckets: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    for (i, stroke) in strokes.iter().enumerate() {
        let b = stroke.bounds;
        if ![b.x0, b.x1, b.y0, b.y1].iter().all(|n| n.is_finite()) {
            continue;
        }
        let (x0, x1) = (
            ((b.x0 - CONNECT_TOL) / CELL).floor() as i64,
            ((b.x1 + CONNECT_TOL) / CELL).floor() as i64,
        );
        let (y0, y1) = (
            ((b.y0 - CONNECT_TOL) / CELL).floor() as i64,
            ((b.y1 + CONNECT_TOL) / CELL).floor() as i64,
        );
        for x in x0..=x1 {
            for y in y0..=y1 {
                buckets.entry((x, y)).or_default().push(i);
            }
        }
    }
    for list in buckets.values() {
        for a in 0..list.len() {
            for b in a + 1..list.len() {
                let (i, j) = (list[a], list[b]);
                let (left, right) = (root(&mut parent, i), root(&mut parent, j));
                if left != right && touches(strokes[i].bounds, strokes[j].bounds) {
                    parent[right] = left;
                }
            }
        }
    }
    let mut grouped: Vec<Vec<usize>> = Vec::new();
    let mut lookup = HashMap::new();
    for i in 0..strokes.len() {
        let r = root(&mut parent, i);
        let index = *lookup.entry(r).or_insert_with(|| {
            grouped.push(Vec::new());
            grouped.len() - 1
        });
        grouped[index].push(i);
    }
    grouped
}

#[derive(Clone, Debug)]
pub(super) struct BoundaryLine {
    pub pos: f64,
    pub segments: Vec<(f64, f64)>,
    pub virtual_line: bool,
}
#[derive(Clone, Debug)]
pub(super) struct TableGrid {
    pub bounds: Rect,
    pub xs: Vec<f64>,
    pub ys: Vec<f64>,
    pub h_lines: Vec<BoundaryLine>,
    pub v_lines: Vec<BoundaryLine>,
    pub low_rank: bool,
}

fn cluster(strokes: &[&Stroke], horizontal: bool) -> Vec<BoundaryLine> {
    let mut ordered = strokes.to_vec();
    let pos = |s: &Stroke| {
        if horizontal {
            s.bounds.center_y()
        } else {
            s.bounds.center_x()
        }
    };
    ordered.sort_by(|a, b| pos(a).total_cmp(&pos(b)));
    let mut lines: Vec<BoundaryLine> = Vec::new();
    for stroke in ordered {
        let p = pos(stroke);
        let segment = if horizontal {
            (stroke.bounds.x0, stroke.bounds.x1)
        } else {
            (stroke.bounds.y0, stroke.bounds.y1)
        };
        if let Some(last) = lines.last_mut().filter(|last| p - last.pos <= 2.0) {
            last.segments.push(segment);
            last.pos = (last.pos + p) / 2.0;
        } else {
            lines.push(BoundaryLine {
                pos: p,
                segments: vec![segment],
                virtual_line: false,
            });
        }
    }
    lines
}
fn coverage(line: &BoundaryLine, lo: f64, hi: f64) -> f64 {
    if hi <= lo {
        return 0.0;
    }
    let mut segments: Vec<_> = line
        .segments
        .iter()
        .map(|&(a, b)| (a.min(b).max(lo), a.max(b).min(hi)))
        .filter(|(a, b)| b > a)
        .collect();
    segments.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut cursor, mut total) = (lo, 0.0);
    for (a, b) in segments {
        let start = a.max(cursor);
        if b > start {
            total += b - start;
            cursor = b;
        }
    }
    total
}
fn frame(mut lines: Vec<BoundaryLine>, lo: f64, hi: f64, segment: (f64, f64)) -> Vec<BoundaryLine> {
    if !lines.iter().any(|line| (line.pos - lo).abs() <= 3.0) {
        lines.push(BoundaryLine {
            pos: lo,
            segments: vec![segment],
            virtual_line: true,
        });
    }
    if !lines.iter().any(|line| (line.pos - hi).abs() <= 3.0) {
        lines.push(BoundaryLine {
            pos: hi,
            segments: vec![segment],
            virtual_line: true,
        });
    }
    lines.sort_by(|a, b| a.pos.total_cmp(&b.pos));
    let mut deduped: Vec<BoundaryLine> = Vec::new();
    for line in lines {
        if let Some(last) = deduped.last_mut().filter(|last| line.pos - last.pos < 3.0) {
            last.segments.extend(line.segments);
            last.virtual_line &= line.virtual_line;
        } else {
            deduped.push(line);
        }
    }
    deduped
}
pub(super) fn solve_grid(group: &[&Stroke], allow_low_rank: bool) -> Option<TableGrid> {
    let h: Vec<_> = group
        .iter()
        .copied()
        .filter(|s| s.orientation == "h")
        .collect();
    let v: Vec<_> = group
        .iter()
        .copied()
        .filter(|s| s.orientation == "v")
        .collect();
    if h.is_empty() || v.is_empty() {
        return None;
    }
    let bounds = group.iter().map(|s| s.bounds).reduce(Rect::union)?;
    if bounds.width() < 24.0 || bounds.height() < 16.0 {
        return None;
    }
    let mut h_lines = frame(
        cluster(&h, true),
        bounds.y0,
        bounds.y1,
        (bounds.x0, bounds.x1),
    );
    let v_lines = frame(
        cluster(&v, false),
        bounds.x0,
        bounds.x1,
        (bounds.y0, bounds.y1),
    );
    let rows = h_lines.len().checked_sub(1)?;
    let cols = v_lines.len().checked_sub(1)?;
    let low_rank = rows < 2 || cols < 2;
    if low_rank {
        if !allow_low_rank || rows < 1 || cols < 1 {
            return None;
        }
        let horizontal = [&h_lines[0], h_lines.last()?]
            .iter()
            .all(|l| !l.virtual_line && coverage(l, bounds.x0, bounds.x1) >= 0.85 * bounds.width());
        let vertical = [&v_lines[0], v_lines.last()?].iter().all(|l| {
            !l.virtual_line && coverage(l, bounds.y0, bounds.y1) >= 0.85 * bounds.height()
        });
        if !horizontal || !vertical {
            return None;
        }
    }
    let covered: f64 = h_lines
        .iter()
        .filter(|l| !l.virtual_line)
        .map(|l| coverage(l, bounds.x0, bounds.x1))
        .sum::<f64>()
        + v_lines
            .iter()
            .filter(|l| !l.virtual_line)
            .map(|l| coverage(l, bounds.y0, bounds.y1))
            .sum::<f64>();
    if covered
        < 0.25 * (h_lines.len() as f64 * bounds.width() + v_lines.len() as f64 * bounds.height())
    {
        return None;
    }
    h_lines.reverse();
    Some(TableGrid {
        bounds,
        xs: v_lines.iter().map(|l| l.pos).collect(),
        ys: h_lines.iter().map(|l| l.pos).collect(),
        h_lines,
        v_lines,
        low_rank,
    })
}

fn white(color: &str) -> bool {
    let bytes = color.as_bytes();
    bytes.len() == 6
        && bytes.iter().enumerate().all(|(i, b)| {
            if i % 2 == 0 {
                b.eq_ignore_ascii_case(&b'F')
            } else {
                b.eq_ignore_ascii_case(&b'F') || b.eq_ignore_ascii_case(&b'E')
            }
        })
}

pub(super) fn solve_lattice_grids(strokes: &[Stroke]) -> Vec<TableGrid> {
    let mut grids = Vec::new();
    for form in [false, true] {
        let subset: Vec<_> = strokes
            .iter()
            .filter(|stroke| stroke.from_form == form)
            .cloned()
            .collect();
        for group in group_strokes(&subset) {
            let refs: Vec<_> = group.iter().map(|&index| &subset[index]).collect();
            if let Some(grid) = solve_grid(&refs, !form) {
                if !form || (grid.ys.len() >= 4 && grid.xs.len() >= 4) {
                    grids.push(grid);
                }
            }
        }
    }
    grids
}

pub(super) fn solve_page_grids(shapes: &PageShapes) -> Vec<TableGrid> {
    let base = solve_lattice_grids(&shapes.strokes);
    let vertical: Vec<_> = shapes
        .strokes
        .iter()
        .filter(|s| s.orientation == "v" && !s.from_form)
        .collect();
    if vertical.len() < 2 {
        return base;
    }
    let mut edges = Vec::new();
    for fill in &shapes.fills {
        if base
            .iter()
            .any(|grid| fill.bounds.overlap_ratio(grid.bounds) >= 0.5)
            || white(&fill.color)
        {
            continue;
        }
        let (w, h) = (fill.bounds.width(), fill.bounds.height());
        if h > 42.0
            || w < 48.0
            || w < 2.0 * h
            || vertical
                .iter()
                .filter(|s| touches(s.bounds, fill.bounds))
                .take(2)
                .count()
                < 2
        {
            continue;
        }
        for y in [fill.bounds.y0, fill.bounds.y1] {
            edges.push(Stroke {
                bounds: Rect {
                    x0: fill.bounds.x0,
                    x1: fill.bounds.x1,
                    y0: y - 0.25,
                    y1: y + 0.25,
                },
                orientation: "h".into(),
                width_pt: 0.5,
                color: fill.color.clone(),
                from_form: false,
            });
        }
        for x in [fill.bounds.x0, fill.bounds.x1] {
            edges.push(Stroke {
                bounds: Rect {
                    x0: x - 0.25,
                    x1: x + 0.25,
                    y0: fill.bounds.y0,
                    y1: fill.bounds.y1,
                },
                orientation: "v".into(),
                width_pt: 0.5,
                color: fill.color.clone(),
                from_form: false,
            });
        }
    }
    if edges.is_empty() {
        return base;
    }
    let mut all = shapes.strokes.clone();
    all.extend(edges);
    solve_lattice_grids(&all)
}

pub(super) fn trim_ghost_edge_columns(
    mut grid: TableGrid,
    chars: &[PdfChar],
    fills: &[Fill],
) -> TableGrid {
    if grid.h_lines.len() < 4 {
        return grid;
    }
    let internal = &grid.h_lines[1..grid.h_lines.len() - 1];
    let is_ghost = |inner: &BoundaryLine, outer: &BoundaryLine, x0: f64, x1: f64| {
        let width = x1 - x0;
        if width <= 0.0 {
            return false;
        }
        let structured = inner.segments.len() >= 2
            && inner.segments.iter().any(|&(a, b)| {
                internal
                    .iter()
                    .any(|h| (a.min(b) - h.pos).abs() <= 2.0 || (a.max(b) - h.pos).abs() <= 2.0)
            });
        if !outer.virtual_line && !structured {
            return false;
        }
        let reach = 2.0_f64.max(0.3 * width);
        if internal
            .iter()
            .any(|line| line.virtual_line || coverage(line, x0, x1) >= reach)
        {
            return false;
        }
        if chars.iter().any(|c| {
            c.code > 0x20
                && c.bounds.center_x() >= x0
                && c.bounds.center_x() <= x1
                && c.bounds.center_y() >= grid.bounds.y0
                && c.bounds.center_y() <= grid.bounds.y1
        }) {
            return false;
        }
        if fills.iter().any(|fill| {
            !white(&fill.color)
                && fill.bounds.area() <= 1.5 * grid.bounds.area()
                && fill.bounds.x1.min(x1) - fill.bounds.x0.max(x0) >= 0.5 * width
                && fill.bounds.y1.min(grid.bounds.y1) - fill.bounds.y0.max(grid.bounds.y0) >= 3.0
        }) {
            return false;
        }
        true
    };
    while grid.xs.len() > 3 {
        let n = grid.xs.len();
        if !is_ghost(
            &grid.v_lines[n - 2],
            &grid.v_lines[n - 1],
            grid.xs[n - 2],
            grid.xs[n - 1],
        ) {
            break;
        }
        grid.xs.pop();
        grid.v_lines.pop();
    }
    while grid.xs.len() > 3 && is_ghost(&grid.v_lines[1], &grid.v_lines[0], grid.xs[0], grid.xs[1])
    {
        grid.xs.remove(0);
        grid.v_lines.remove(0);
    }
    grid.bounds.x0 = grid.xs[0];
    grid.bounds.x1 = *grid.xs.last().expect("grid has boundaries");
    grid
}

fn clusters(mut positions: Vec<f64>) -> Vec<(f64, usize)> {
    positions.sort_by(f64::total_cmp);
    let mut out: Vec<(f64, usize)> = Vec::new();
    for pos in positions {
        if let Some(last) = out.last_mut().filter(|last| pos - last.0 <= 2.0) {
            last.0 = (last.0 + pos) / 2.0;
            last.1 += 1;
        } else {
            out.push((pos, 1));
        }
    }
    out
}

fn augment_rows(grid: &mut TableGrid, chars: &[PdfChar]) {
    let ends: Vec<_> = grid
        .v_lines
        .iter()
        .filter(|line| !line.virtual_line)
        .flat_map(|line| {
            line.segments
                .iter()
                .flat_map(|&(a, b)| [a.min(b), a.max(b)])
        })
        .collect();
    let mut kept = Vec::new();
    for (pos, count) in clusters(ends) {
        if count < 3
            || pos <= grid.bounds.y0 + 3.0
            || pos >= grid.bounds.y1 - 3.0
            || grid.ys.iter().any(|y| (y - pos).abs() < 3.0)
            || chars.iter().any(|c| {
                c.code > 0x20
                    && c.bounds.y0 < pos - 0.3 * c.bounds.height()
                    && c.bounds.y1 > pos + 0.3 * c.bounds.height()
            })
            || kept.iter().any(|other: &f64| (*other - pos).abs() < 3.0)
        {
            continue;
        }
        kept.push(pos);
        grid.h_lines.push(BoundaryLine {
            pos,
            segments: vec![(grid.bounds.x0, grid.bounds.x1)],
            virtual_line: true,
        });
    }
    grid.h_lines.sort_by(|a, b| b.pos.total_cmp(&a.pos));
    grid.ys = grid.h_lines.iter().map(|line| line.pos).collect();
}

fn augment_columns(grid: &mut TableGrid, chars: &[PdfChar], fills: &[Fill]) {
    if grid.xs.len() > 2 {
        return;
    }
    let inside: Vec<_> = fills
        .iter()
        .filter(|f| {
            f.bounds.x0 >= grid.bounds.x0 - 2.0
                && f.bounds.x1 <= grid.bounds.x1 + 2.0
                && f.bounds.y0 >= grid.bounds.y0 - 2.0
                && f.bounds.y1 <= grid.bounds.y1 + 2.0
        })
        .collect();
    let slivers: Vec<_> = inside
        .iter()
        .copied()
        .filter(|f| f.bounds.width() < 14.0)
        .collect();
    let mut bodies: Vec<_> = inside
        .iter()
        .filter(|f| f.bounds.width() >= 14.0)
        .map(|f| (f.color.as_str(), f.bounds))
        .collect();
    for (color, bounds) in &mut bodies {
        loop {
            let mut grew = false;
            for fill in &slivers {
                if fill.color != *color
                    || (fill.bounds.y0 - bounds.y0).abs() > 2.0
                    || (fill.bounds.y1 - bounds.y1).abs() > 2.0
                    || fill.bounds.x0 > bounds.x1 + 2.0
                    || fill.bounds.x1 < bounds.x0 - 2.0
                {
                    continue;
                }
                let (x0, x1) = (bounds.x0.min(fill.bounds.x0), bounds.x1.max(fill.bounds.x1));
                if x0 != bounds.x0 || x1 != bounds.x1 {
                    bounds.x0 = x0;
                    bounds.x1 = x1;
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
    }
    let edges = bodies.iter().flat_map(|(_, b)| [b.x0, b.x1]).collect();
    let visible: Vec<_> = chars.iter().filter(|c| c.code > 0x20).collect();
    let mut lines: HashMap<i64, Vec<&PdfChar>> = HashMap::new();
    for c in &visible {
        lines
            .entry((c.origin_y / 2.0 + 0.5).floor() as i64)
            .or_default()
            .push(c);
    }
    for line in lines.values_mut() {
        line.sort_by(|a, b| a.bounds.x0.total_cmp(&b.bounds.x0));
    }
    for (pos, count) in clusters(edges) {
        if count < 2
            || pos <= grid.bounds.x0 + 3.0
            || pos >= grid.bounds.x1 - 3.0
            || grid.xs.iter().any(|x| (x - pos).abs() < 3.0)
        {
            continue;
        }
        let crossed = visible.iter().any(|c| {
            c.bounds.x0 < pos - 0.3 * c.bounds.width() && c.bounds.x1 > pos + 0.3 * c.bounds.width()
        });
        let flowing = lines.values().any(|line| {
            line.windows(2).any(|pair| {
                pair[0].bounds.x1 <= pos
                    && pair[1].bounds.x0 >= pos
                    && pair[1].bounds.x0 - pair[0].bounds.x1 < 0.4 * pair[0].font_size
            })
        });
        if !crossed && !flowing {
            grid.v_lines.push(BoundaryLine {
                pos,
                segments: vec![(grid.bounds.y0, grid.bounds.y1)],
                virtual_line: true,
            });
        }
    }
    grid.v_lines.sort_by(|a, b| a.pos.total_cmp(&b.pos));
    grid.xs = grid.v_lines.iter().map(|line| line.pos).collect();
}

fn split_run(xs: &[f64], col: usize, span: usize, chars: &[PdfChar]) -> Option<Vec<Vec<PdfChar>>> {
    let mut parts = vec![Vec::new(); span];
    for c in chars {
        if c.code > 0x20
            && (1..span).any(|b| {
                c.bounds.x0 < xs[col + b] - 0.3 * c.bounds.width()
                    && c.bounds.x1 > xs[col + b] + 0.3 * c.bounds.width()
            })
        {
            return None;
        }
        let target = (1..span)
            .find(|&b| c.bounds.center_x() < xs[col + b])
            .map_or(span - 1, |b| b - 1);
        parts[target].push(c.clone());
    }
    if parts
        .iter()
        .filter(|part| part.iter().any(|c| c.code > 0x20))
        .count()
        < 2
    {
        return None;
    }
    let visible: Vec<_> = chars.iter().filter(|c| c.code > 0x20).collect();
    let gap = 0.5 * visible.iter().map(|c| c.bounds.height()).sum::<f64>() / visible.len() as f64;
    for b in 1..span {
        let x = xs[col + b];
        let left = visible
            .iter()
            .filter(|c| c.bounds.center_x() < x)
            .map(|c| c.bounds.x1)
            .fold(f64::NEG_INFINITY, f64::max);
        let right = visible
            .iter()
            .filter(|c| c.bounds.center_x() >= x)
            .map(|c| c.bounds.x0)
            .fold(f64::INFINITY, f64::min);
        if left.is_finite() && right.is_finite() && right - left < gap {
            return None;
        }
    }
    Some(parts)
}

pub(super) fn detect_cell_v_align(bounds: Rect, chars: &[PdfChar]) -> Option<&'static str> {
    let min = chars
        .iter()
        .filter(|c| c.code > 0x20)
        .map(|c| c.bounds.y0)
        .fold(f64::INFINITY, f64::min);
    let max = chars
        .iter()
        .filter(|c| c.code > 0x20)
        .map(|c| c.bounds.y1)
        .fold(f64::NEG_INFINITY, f64::max);
    if max <= min || bounds.height() <= (max - min) * 1.25 {
        return None;
    }
    let tol = 1.5_f64.max(0.08 * bounds.height());
    if ((min + max) / 2.0 - bounds.center_y()).abs() < tol {
        Some("center")
    } else if min - bounds.y0 < tol {
        Some("bottom")
    } else {
        None
    }
}

fn cell_fill(bounds: Rect, fills: &[Fill], table_area: f64) -> Option<String> {
    fills
        .iter()
        .filter(|fill| {
            fill.bounds.area() <= 1.5 * table_area
                && !white(&fill.color)
                && bounds.overlap_ratio(fill.bounds) > 0.5
        })
        .next_back()
        .map(|fill| fill.color.clone())
}

fn border_color(bounds: Rect, strokes: &[Stroke]) -> Option<String> {
    let mut colors: Vec<(&str, f64)> = Vec::new();
    let mut total = 0.0;
    for stroke in strokes {
        let b = stroke.bounds;
        if stroke.from_form
            || b.center_x() < bounds.x0 - CONNECT_TOL
            || b.center_x() > bounds.x1 + CONNECT_TOL
            || b.center_y() < bounds.y0 - CONNECT_TOL
            || b.center_y() > bounds.y1 + CONNECT_TOL
        {
            continue;
        }
        let length = b.width().max(b.height());
        total += length;
        if let Some((_, weight)) = colors
            .iter_mut()
            .find(|(name, _)| *name == stroke.color.as_str())
        {
            *weight += length;
        } else {
            colors.push((&stroke.color, length));
        }
    }
    let mut best = None;
    let mut weight = 0.0;
    for (color, length) in colors {
        if length > weight {
            best = Some(color);
            weight = length;
        }
    }
    let color = best?;
    if total <= 0.0 || weight / total < 0.8 || color == "000000" {
        None
    } else {
        Some(color.to_owned())
    }
}

pub(super) struct CellLayout {
    pub rows: usize,
    pub cols: usize,
    pub anchor_of: Vec<Vec<(usize, usize)>>,
    pub span_of: HashMap<(usize, usize), (usize, usize)>,
}

fn border_present(line: &BoundaryLine, lo: f64, hi: f64) -> bool {
    line.virtual_line || coverage(line, lo, hi) >= 0.5 * (hi - lo)
}

pub(super) fn layout_cells(grid: &TableGrid) -> CellLayout {
    let rows = grid.ys.len() - 1;
    let cols = grid.xs.len() - 1;
    let mut anchor_of = vec![vec![(0, 0); cols]; rows];
    let mut taken = vec![vec![false; cols]; rows];
    let mut span_of = HashMap::new();
    for r in 0..rows {
        for c in 0..cols {
            if taken[r][c] {
                continue;
            }
            let mut col_span = 1;
            while c + col_span < cols
                && !taken[r][c + col_span]
                && !border_present(&grid.v_lines[c + col_span], grid.ys[r + 1], grid.ys[r])
            {
                col_span += 1;
            }
            let mut row_span = 1;
            while r + row_span < rows {
                let below = r + row_span;
                if border_present(&grid.h_lines[below], grid.xs[c], grid.xs[c + col_span])
                    || (1..col_span).any(|k| {
                        border_present(&grid.v_lines[c + k], grid.ys[below + 1], grid.ys[below])
                    })
                    || (0..col_span).any(|k| taken[below][c + k])
                {
                    break;
                }
                row_span += 1;
            }
            span_of.insert((r, c), (col_span, row_span));
            for dr in 0..row_span {
                for dc in 0..col_span {
                    taken[r + dr][c + dc] = true;
                    anchor_of[r + dr][c + dc] = (r, c);
                }
            }
        }
    }
    CellLayout {
        rows,
        cols,
        anchor_of,
        span_of,
    }
}

pub(super) struct DetectedTables {
    pub tables: Vec<TableBlock>,
    pub remaining_chars: Vec<PdfChar>,
}

pub(super) fn detect_tables(
    shapes: &PageShapes,
    chars: &[PdfChar],
    page_height: Option<f64>,
    page_width: Option<f64>,
    build_blocks: &impl Fn(&[PdfChar]) -> Result<Vec<TextBlock>, String>,
) -> Result<DetectedTables, String> {
    let mut grids: Vec<_> = solve_page_grids(shapes).into_iter().map(|grid| {
        if grid.low_rank { grid } else { trim_ghost_edge_columns(grid, chars, &shapes.fills) }
    }).filter(|grid| !matches!((page_width,page_height),(Some(w),Some(h)) if grid.bounds.width() >= 0.96*w && grid.bounds.height() >= 0.96*h)).collect();
    let all_bounds: Vec<_> = grids.iter().map(|grid| grid.bounds).collect();
    let contains = |bounds: Rect, x: f64, y: f64| {
        x >= bounds.x0 && x <= bounds.x1 && y >= bounds.y0 && y <= bounds.y1
    };
    let mut position = 0usize;
    grids.retain(|grid| {
        let index = position;
        position += 1;
        if !grid.low_rank {
            return true;
        }
        if matches!((page_width,page_height),(Some(w),Some(h)) if w > h)
            || page_height.is_some_and(|h| grid.bounds.height() > 0.55 * h)
            || grid.bounds.height() < 38.0
        {
            return false;
        }
        if all_bounds
            .iter()
            .enumerate()
            .any(|(i, b)| i != index && contains(grid.bounds, b.center_x(), b.center_y()))
        {
            return false;
        }
        chars
            .iter()
            .filter(|c| {
                c.code > 0x20 && contains(grid.bounds, c.bounds.center_x(), c.bounds.center_y())
            })
            .take(4)
            .count()
            >= 4
    });
    let mut order: Vec<_> = (0..grids.len()).collect();
    order.sort_by(|&a, &b| grids[a].bounds.area().total_cmp(&grids[b].bounds.area()));
    let mut per_grid: Vec<Vec<PdfChar>> = vec![Vec::new(); grids.len()];
    let mut remaining_chars = Vec::new();
    for c in chars {
        if let Some(&index) = order
            .iter()
            .find(|&&i| contains(grids[i].bounds, c.bounds.center_x(), c.bounds.center_y()))
        {
            per_grid[index].push(c.clone());
        } else {
            remaining_chars.push(c.clone());
        }
    }
    let mut tables = Vec::new();
    for (mut grid, inside) in grids.into_iter().zip(per_grid) {
        if !grid.low_rank {
            augment_rows(&mut grid, &inside);
            augment_columns(&mut grid, &inside, &shapes.fills);
        } else {
            augment_columns(&mut grid, &inside, &shapes.fills);
            if grid.xs.len() >= 3 && grid.ys.len() >= 3 {
                grid.low_rank = false;
                augment_rows(&mut grid, &inside);
            }
        }
        let layout = layout_cells(&grid);
        let mut cell_chars: HashMap<(usize, usize), Vec<PdfChar>> = HashMap::new();
        for c in inside {
            let cy = c.bounds.center_y();
            let cx = c.bounds.center_x();
            let row = grid
                .ys
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, y)| cy >= **y)
                .map(|(i, _)| i - 1)
                .unwrap_or(if cy > grid.ys[0] { 0 } else { layout.rows - 1 })
                .min(layout.rows - 1);
            let col = grid
                .xs
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, x)| cx <= **x)
                .map(|(i, _)| i - 1)
                .unwrap_or(if cx < grid.xs[0] { 0 } else { layout.cols - 1 })
                .min(layout.cols - 1);
            cell_chars
                .entry(layout.anchor_of[row][col])
                .or_default()
                .push(c);
        }
        let mut rows = Vec::new();
        for r in 0..layout.rows {
            let mut row = Vec::new();
            for c in 0..layout.cols {
                let anchor = layout.anchor_of[r][c];
                if anchor.1 != c {
                    continue;
                }
                let (cols, height) = layout.span_of[&anchor];
                let bounds = Rect {
                    x0: grid.xs[c],
                    x1: grid.xs[c + cols],
                    y0: grid.ys[anchor.0 + height],
                    y1: grid.ys[anchor.0],
                };
                let mut cell = TableCell {
                    bounds,
                    grid_span: cols,
                    v_merge: None,
                    fill: None,
                    v_align: None,
                    soft_edges: None,
                    blocks: Vec::new(),
                };
                if anchor.0 == r {
                    if height > 1 {
                        cell.v_merge = Some("restart".into());
                    }
                    let content = cell_chars.get(&(r, c)).map(Vec::as_slice).unwrap_or(&[]);
                    if height == 1 && cols > 1 && !content.is_empty() {
                        if let Some(parts) = split_run(&grid.xs, c, cols, content) {
                            for (k, part) in parts.iter().enumerate() {
                                let part_bounds = Rect {
                                    x0: grid.xs[c + k],
                                    x1: grid.xs[c + k + 1],
                                    y0: bounds.y0,
                                    y1: bounds.y1,
                                };
                                row.push(TableCell {
                                    bounds: part_bounds,
                                    grid_span: 1,
                                    v_merge: None,
                                    fill: cell_fill(part_bounds, &shapes.fills, grid.bounds.area()),
                                    v_align: detect_cell_v_align(part_bounds, part)
                                        .map(str::to_owned),
                                    soft_edges: Some(SoftEdges {
                                        left: k > 0,
                                        right: k + 1 < cols,
                                        top: false,
                                        bottom: false,
                                    }),
                                    blocks: if part.is_empty() {
                                        Vec::new()
                                    } else {
                                        build_blocks(part)?
                                    },
                                });
                            }
                            continue;
                        }
                    }
                    if !content.is_empty() {
                        cell.blocks = build_blocks(content)?;
                        cell.v_align = detect_cell_v_align(bounds, content).map(str::to_owned);
                    }
                    cell.fill = cell_fill(bounds, &shapes.fills, grid.bounds.area());
                } else {
                    cell.v_merge = Some("continue".into());
                }
                row.push(cell);
            }
            rows.push(row);
        }
        tables.push(TableBlock {
            bounds: grid.bounds,
            col_widths_pt: grid.xs.windows(2).map(|pair| pair[1] - pair[0]).collect(),
            rows,
            confidence: None,
            border_color: border_color(grid.bounds, &shapes.strokes),
            form: false,
            separator_rule: None,
            spacing_before_pt: None,
        });
    }
    tables.sort_by(|a, b| b.bounds.y1.total_cmp(&a.bounds.y1));
    Ok(DetectedTables {
        tables,
        remaining_chars,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(x0: f64, y0: f64, x1: f64, y1: f64, orientation: &str) -> Stroke {
        Stroke {
            bounds: Rect { x0, y0, x1, y1 },
            orientation: orientation.into(),
            width_pt: 0.5,
            color: "000000".into(),
            from_form: false,
        }
    }

    #[test]
    fn group_and_solve_a_two_by_two_grid() {
        let strokes = vec![
            stroke(0.0, 0.0, 80.0, 0.0, "h"),
            stroke(0.0, 20.0, 80.0, 20.0, "h"),
            stroke(0.0, 40.0, 80.0, 40.0, "h"),
            stroke(0.0, 0.0, 0.0, 40.0, "v"),
            stroke(40.0, 0.0, 40.0, 40.0, "v"),
            stroke(80.0, 0.0, 80.0, 40.0, "v"),
        ];
        let groups = group_strokes(&strokes);
        assert_eq!(groups, vec![vec![0, 1, 2, 3, 4, 5]]);
        let grid = solve_grid(&strokes.iter().collect::<Vec<_>>(), false).unwrap();
        assert_eq!(grid.xs, vec![0.0, 40.0, 80.0]);
        assert_eq!(grid.ys, vec![40.0, 20.0, 0.0]);
        let cells = layout_cells(&grid);
        assert_eq!(cells.span_of[&(0, 0)], (1, 1));
        assert_eq!(cells.span_of.len(), 4);
        let shapes = PageShapes {
            strokes,
            fills: Vec::new(),
            ignored_paths: 0,
            curved_fills: Vec::new(),
        };
        let detected =
            detect_tables(&shapes, &[], Some(800.0), Some(600.0), &|_| Ok(Vec::new())).unwrap();
        assert_eq!(detected.tables.len(), 1);
        assert_eq!(detected.tables[0].col_widths_pt, vec![40.0, 40.0]);
        assert_eq!(detected.tables[0].rows.len(), 2);
    }
}
