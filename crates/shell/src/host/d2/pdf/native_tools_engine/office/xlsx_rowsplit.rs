//! Split multiline statement bands into data rows using shared line starts.
use super::geometry::Rect;
use super::ir::{Line, TableBlock, TableCell, TextBlock};
use std::collections::{BTreeSet, HashMap};
#[derive(Clone, Copy)]
struct Entry<'a> {
    line: &'a Line,
    src: &'a TextBlock,
    col: usize,
}
fn lines(cell: &TableCell, col: usize) -> Vec<Entry<'_>> {
    cell.blocks
        .iter()
        .flat_map(|b| {
            b.lines
                .iter()
                .filter(|l| l.spans.iter().any(|s| !jscompat::trim(&s.text).is_empty()))
                .map(move |line| Entry { line, src: b, col })
        })
        .collect()
}
struct Cluster<'a> {
    top: f64,
    cols: BTreeSet<usize>,
    members: Vec<Entry<'a>>,
}
fn split(row: &[TableCell]) -> Option<Vec<Vec<TableCell>>> {
    if row.iter().any(|c| c.grid_span > 1 || c.v_merge.is_some()) {
        return None;
    }
    let per: Vec<_> = row.iter().enumerate().map(|(i, c)| lines(c, i)).collect();
    let bearing = per.iter().filter(|v| !v.is_empty()).count();
    if bearing < 2 || per.iter().map(Vec::len).max().unwrap_or(0) < 4 {
        return None;
    }
    let mut all: Vec<_> = per.iter().flatten().copied().collect();
    all.sort_by(|a, b| b.line.bounds.y1.total_cmp(&a.line.bounds.y1));
    let mut clusters: Vec<Cluster<'_>> = Vec::new();
    for e in &all {
        if let Some(last) = clusters
            .last_mut()
            .filter(|c| c.top - e.line.bounds.y1 <= 2.0)
        {
            last.cols.insert(e.col);
            last.members.push(*e);
        } else {
            clusters.push(Cluster {
                top: e.line.bounds.y1,
                cols: BTreeSet::from([e.col]),
                members: vec![*e],
            });
        }
    }
    let mut bounds: Vec<Cluster<'_>> = Vec::new();
    for c in clusters {
        if c.cols.len() < bearing.div_ceil(2).max(2) {
            continue;
        }
        if bounds.last().is_some_and(|last| last.top - c.top < 6.0) {
            continue;
        }
        bounds.push(c);
    }
    if bounds.len() < 3 {
        return None;
    }
    let mut membership = HashMap::new();
    for (i, c) in bounds.iter().enumerate() {
        for m in &c.members {
            membership.insert(m.line as *const Line, i);
        }
    }
    let index = |line: &Line| -> isize {
        if let Some(i) = membership.get(&(line as *const Line)) {
            return *i as isize;
        }
        for (i, b) in bounds.iter().enumerate() {
            if line.bounds.y1 > b.top + 2.0 {
                return i as isize - 1;
            }
        }
        bounds.len() as isize - 1
    };
    let lead = usize::from(all.iter().any(|e| index(e.line) < 0));
    let total = bounds.len() + lead;
    let top = row
        .iter()
        .map(|c| c.bounds.y1)
        .fold(f64::NEG_INFINITY, f64::max);
    let bottom = row
        .iter()
        .map(|c| c.bounds.y0)
        .fold(f64::INFINITY, f64::min);
    let mut out = Vec::new();
    for i in 0..total {
        let y1 = if i == 0 { top } else { bounds[i - lead].top };
        let next = i as isize - lead as isize + 1;
        let y0 = if next < (bounds.len() as isize) {
            bounds[next as usize].top
        } else {
            bottom
        };
        let mut cells = Vec::new();
        for (col, cell) in row.iter().enumerate() {
            let mut soft = cell.soft_edges.clone().unwrap_or_default();
            if i > 0 {
                soft.top = true;
            }
            if i + 1 < total {
                soft.bottom = true;
            }
            let mut sub = TableCell {
                bounds: Rect {
                    x0: cell.bounds.x0,
                    x1: cell.bounds.x1,
                    y0,
                    y1,
                },
                grid_span: cell.grid_span,
                v_merge: None,
                fill: cell.fill.clone(),
                v_align: cell.v_align.clone(),
                soft_edges: Some(soft),
                blocks: Vec::new(),
            };
            let selected: Vec<_> = per[col]
                .iter()
                .filter(|e| index(e.line) + lead as isize == i as isize)
                .copied()
                .collect();
            let mut groups: Vec<(&TextBlock, Vec<&Line>)> = Vec::new();
            for entry in selected {
                if let Some((src, ls)) = groups
                    .last_mut()
                    .filter(|(src, _)| std::ptr::eq(*src, entry.src))
                {
                    let _ = src;
                    ls.push(entry.line);
                } else {
                    groups.push((entry.src, vec![entry.line]));
                }
            }
            for (src, ls) in groups {
                let owned: Vec<_> = src
                    .lines
                    .iter()
                    .filter(|l| l.spans.iter().any(|s| !jscompat::trim(&s.text).is_empty()))
                    .collect();
                let first = owned
                    .first()
                    .is_some_and(|l| ls.iter().any(|x| std::ptr::eq(*l, *x)));
                let last = owned
                    .last()
                    .is_some_and(|l| ls.iter().any(|x| std::ptr::eq(*l, *x)));
                let mut box_ = Rect {
                    x0: f64::INFINITY,
                    y0: f64::INFINITY,
                    x1: f64::NEG_INFINITY,
                    y1: f64::NEG_INFINITY,
                };
                for l in &ls {
                    box_.x0 = box_.x0.min(l.bounds.x0);
                    box_.y0 = box_.y0.min(l.bounds.y0);
                    box_.x1 = box_.x1.max(l.bounds.x1);
                    box_.y1 = box_.y1.max(l.bounds.y1);
                }
                sub.blocks.push(TextBlock {
                    lines: ls.into_iter().cloned().collect(),
                    bounds: box_,
                    align: src.align,
                    first_line_indent_pt: 0.0,
                    direction: src.direction,
                    list: if first { src.list.clone() } else { None },
                    stitched_from_line: None,
                    toc_entry: if last { src.toc_entry.clone() } else { None },
                    spacing_before_pt: None,
                    border: None,
                    card_id: None,
                });
            }
            cells.push(sub);
        }
        out.push(cells);
    }
    Some(out)
}
pub(super) fn split_bands(block: &TableBlock) -> TableBlock {
    let mut rows = Vec::new();
    for row in &block.rows {
        if let Some(s) = split(row) {
            rows.extend(s);
        } else {
            rows.push(row.clone());
        }
    }
    let mut result = block.clone();
    result.rows = rows;
    result
}
