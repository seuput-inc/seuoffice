//! Merge adjacent lattice panels while retaining cell spans and soft edges.
use super::super::geometry::Rect;
use super::super::ir::{SoftEdges, TableBlock, TableCell};
fn row_boundaries(t: &TableBlock) -> Option<Vec<f64>> {
    let n = t.rows.len();
    if n == 0 {
        return None;
    }
    let mut ys = vec![None; n + 1];
    ys[0] = Some(t.bounds.y1);
    ys[n] = Some(t.bounds.y0);
    for r in 0..n {
        for c in &t.rows[r] {
            if c.v_merge.as_deref() == Some("continue") {
                continue;
            }
            if ys[r].is_none() {
                ys[r] = Some(c.bounds.y1);
            }
            let mut span = 1;
            while r + span < n
                && t.rows[r + span].iter().any(|p| {
                    p.v_merge.as_deref() == Some("continue")
                        && (p.bounds.y0 - c.bounds.y0).abs() < 0.1
                        && (p.bounds.x0 - c.bounds.x0).abs() < 0.1
                })
            {
                span += 1;
            }
            if ys[r + span].is_none() {
                ys[r + span] = Some(c.bounds.y0);
            }
        }
    }
    for i in 1..n {
        if ys[i].is_some() {
            continue;
        }
        let mut hi = i + 1;
        while ys[hi].is_none() {
            hi += 1;
        }
        let lo = i - 1;
        let start = ys[lo]?;
        let step = (ys[hi]? - start) / (hi - lo) as f64;
        for (k, slot) in ys.iter_mut().enumerate().take(hi).skip(lo + 1) {
            *slot = Some(start + step * (k - lo) as f64);
        }
    }
    let ys: Vec<_> = ys.into_iter().collect::<Option<_>>()?;
    if ys.windows(2).any(|p| p[1] >= p[0] - 0.1) {
        return None;
    }
    Some(ys)
}
fn snap(v: &[f64], value: f64) -> usize {
    (0..v.len())
        .min_by(|&a, &b| (v[a] - value).abs().total_cmp(&(v[b] - value).abs()))
        .unwrap_or(0)
}
fn merge_group(group: &[&TableBlock]) -> Option<TableBlock> {
    let mut panels = Vec::new();
    for t in group {
        let ys = row_boundaries(t)?;
        if t.rows.len() < 2 {
            return None;
        }
        let mut xs = vec![t.bounds.x0];
        for w in &t.col_widths_pt {
            xs.push(xs.last()? + w);
        }
        panels.push((*t, ys, xs));
    }
    let mut xs = vec![panels.first()?.2[0]];
    for (_, _, edges) in &panels {
        for &x in edges {
            if x > *xs.last()? + 1.0 {
                xs.push(x);
            }
        }
    }
    let mut all_y: Vec<_> = panels.iter().flat_map(|p| p.1.iter().copied()).collect();
    all_y.sort_by(|a, b| b.total_cmp(a));
    let mut ys: Vec<f64> = Vec::new();
    for y in all_y {
        if ys.last().is_none_or(|last| y < *last - 1.0) {
            ys.push(y);
        }
    }
    if xs.len() < 3 || ys.len() < 3 {
        return None;
    }
    struct Place<'a> {
        cell: &'a TableCell,
        c0: usize,
        c1: usize,
        r0: usize,
        r1: usize,
    }
    let mut placed = Vec::new();
    for (t, _, edges) in &panels {
        let lo = snap(&xs, edges[0]);
        let hi = snap(&xs, *edges.last()?);
        for cell in t.rows.iter().flatten() {
            if cell.v_merge.as_deref() == Some("continue") {
                continue;
            }
            let (c0, c1, r0, r1) = (
                snap(&xs, cell.bounds.x0),
                snap(&xs, cell.bounds.x1),
                snap(&ys, cell.bounds.y1),
                snap(&ys, cell.bounds.y0),
            );
            if c1 <= c0 || r1 <= r0 || c0 < lo || c1 > hi {
                return None;
            }
            placed.push(Place {
                cell,
                c0,
                c1,
                r0,
                r1,
            });
        }
    }
    let mut owner = vec![vec![None; xs.len() - 1]; ys.len() - 1];
    for (i, p) in placed.iter().enumerate() {
        for row in owner.iter_mut().take(p.r1).skip(p.r0) {
            for slot in row.iter_mut().take(p.c1).skip(p.c0) {
                if slot.is_some() {
                    return None;
                }
                *slot = Some(i);
            }
        }
    }
    let mut rows = Vec::new();
    for r in 0..ys.len() - 1 {
        let mut row = Vec::new();
        let mut c = 0;
        while c < xs.len() - 1 {
            if let Some(i) = owner[r][c] {
                let p = &placed[i];
                if c != p.c0 {
                    return None;
                }
                let span = p.c1 - p.c0;
                if r == p.r0 {
                    let mut cell = p.cell.clone();
                    cell.grid_span = span;
                    cell.v_merge = if p.r1 - p.r0 > 1 {
                        Some("restart".into())
                    } else {
                        None
                    };
                    row.push(cell);
                } else {
                    row.push(TableCell {
                        bounds: p.cell.bounds,
                        grid_span: span,
                        v_merge: Some("continue".into()),
                        fill: None,
                        v_align: None,
                        soft_edges: None,
                        blocks: vec![],
                    });
                }
                c = p.c1;
            } else {
                let mut end = c;
                while end < xs.len() - 1 && owner[r][end].is_none() {
                    end += 1;
                }
                row.push(TableCell {
                    bounds: Rect {
                        x0: xs[c],
                        x1: xs[end],
                        y0: ys[r + 1],
                        y1: ys[r],
                    },
                    grid_span: end - c,
                    v_merge: None,
                    fill: None,
                    v_align: None,
                    soft_edges: Some(SoftEdges {
                        left: true,
                        right: true,
                        top: true,
                        bottom: true,
                    }),
                    blocks: vec![],
                });
                c = end;
            }
        }
        if !row.iter().any(|c| c.v_merge.is_none()) {
            return None;
        }
        rows.push(row);
    }
    Some(TableBlock {
        bounds: group.iter().map(|t| t.bounds).reduce(Rect::union)?,
        col_widths_pt: xs.windows(2).map(|v| v[1] - v[0]).collect(),
        rows,
        confidence: None,
        border_color: group.iter().find_map(|t| t.border_color.clone()),
        form: false,
        separator_rule: None,
        spacing_before_pt: None,
    })
}
pub(super) fn merge_side_by_side_panels(tables: Vec<TableBlock>) -> (Vec<TableBlock>, Vec<String>) {
    let mut sorted: Vec<_> = tables
        .iter()
        .enumerate()
        .filter(|(_, t)| t.confidence.is_none())
        .map(|(i, _)| i)
        .collect();
    sorted.sort_by(|&a, &b| tables[a].bounds.x0.total_cmp(&tables[b].bounds.x0));
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for i in sorted {
        let target = groups.iter().position(|g| {
            let a = tables[*g.last().unwrap()].bounds;
            let b = tables[i].bounds;
            let gap = b.x0 - a.x1;
            let overlap = (a.y1.min(b.y1) - a.y0.max(b.y0)) / a.height().min(b.height()).max(1.0);
            gap > -0.5 && gap <= 16.0 && overlap >= 0.5
        });
        if let Some(g) = target {
            groups[g].push(i);
        } else {
            groups.push(vec![i]);
        }
    }
    let mut replaced = std::collections::HashMap::new();
    let mut notes = Vec::new();
    for group in groups.into_iter().filter(|g| g.len() >= 2) {
        let refs: Vec<_> = group.iter().map(|&i| &tables[i]).collect();
        if let Some(merged) = merge_group(&refs) {
            replaced.insert(group[0], Some(merged));
            for &i in &group[1..] {
                replaced.insert(i, None);
            }
            notes.push(format!(
                "{} side-by-side panels merged into one grid",
                group.len()
            ));
        }
    }
    let out = tables
        .into_iter()
        .enumerate()
        .filter_map(|(i, t)| replaced.remove(&i).unwrap_or(Some(t)))
        .collect();
    (out, notes)
}
