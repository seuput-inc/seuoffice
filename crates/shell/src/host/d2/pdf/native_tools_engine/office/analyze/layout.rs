//! Assemble reading-order text and positioned blocks into page columns.
use super::super::chars;
use super::super::geometry::Rect;
use super::super::ir::{Direction, ImageBlock, Line, PageBlock, PageColumn, PageSection, PdfChar};
use super::{blocks, columns, lists, toc, units};
fn bounds(b: &PageBlock) -> Rect {
    match b {
        PageBlock::Text(b) => b.bounds,
        PageBlock::Image(b) => b.bounds,
        PageBlock::Table(b) => b.bounds,
    }
}
pub(super) fn merge_blocks(mut text: Vec<PageBlock>, others: Vec<PageBlock>) -> Vec<PageBlock> {
    for b in others {
        let i = text
            .iter()
            .position(|other| bounds(other).y1 < bounds(&b).y1)
            .unwrap_or(text.len());
        text.insert(i, b);
    }
    text
}
pub(super) fn single_section(blocks: Vec<PageBlock>) -> Vec<PageSection> {
    if blocks.is_empty() {
        return Vec::new();
    }
    let dir = blocks
        .iter()
        .find_map(|b| match b {
            PageBlock::Text(t) => Some(t.direction),
            _ => None,
        })
        .unwrap_or(Direction::Ltr);
    let bounds = blocks
        .iter()
        .map(bounds)
        .reduce(Rect::union)
        .unwrap_or_default();
    vec![PageSection {
        bounds,
        columns: vec![PageColumn { bounds, blocks }],
        gutter_widths_pt: vec![],
        direction: dir,
        column_separator: false,
    }]
}
fn unit_runs(row: &[units::LineUnit]) -> Vec<Vec<PdfChar>> {
    let mut sorted: Vec<_> = row.iter().collect();
    sorted.sort_by(|a, b| a.bounds.x0.total_cmp(&b.bounds.x0));
    let mut result: Vec<Vec<PdfChar>> = Vec::new();
    let mut previous: Option<&units::LineUnit> = None;
    for u in sorted {
        let gap = previous.map_or(0.0, |p| u.bounds.x0 - p.bounds.x1);
        let em = previous
            .map_or(0.0, |p| p.font_size)
            .max(u.font_size)
            .max(1.0);
        if previous.is_none() || gap > 2.5 * em {
            result.push(u.chars.clone());
        } else {
            result.last_mut().unwrap().extend_from_slice(&u.chars);
        }
        previous = Some(u);
    }
    result
}
fn stack_lines(lines: Vec<Line>) -> Vec<Vec<Line>> {
    let mut lines = lines;
    lines.sort_by(|a, b| {
        b.baseline
            .total_cmp(&a.baseline)
            .then_with(|| a.bounds.x0.total_cmp(&b.bounds.x0))
    });
    let mut stacks: Vec<Vec<Line>> = Vec::new();
    for line in lines {
        let h = line.bounds.height().max(1.0);
        let mut best: Option<(usize, f64)> = None;
        for (i, stack) in stacks.iter().enumerate() {
            let prev = stack.last().unwrap();
            if line.bounds.y1 > prev.bounds.y1 - 0.5 || prev.bounds.y0 - line.bounds.y1 > 1.5 * h {
                continue;
            }
            let overlap = line.bounds.x1.min(prev.bounds.x1) - line.bounds.x0.max(prev.bounds.x0);
            if overlap < 0.3 * line.bounds.width().min(prev.bounds.width()).max(1.0) {
                continue;
            }
            if best.is_none_or(|(_, old)| overlap > old) {
                best = Some((i, overlap));
            }
        }
        if let Some((i, _)) = best {
            stacks[i].push(line);
        } else {
            stacks.push(vec![line]);
        }
    }
    stacks
}
pub(super) fn assemble_column(
    column: &columns::LayoutColumn<'_>,
    single: bool,
    page_width: f64,
    seq: &mut lists::ListSeq,
    landscape: bool,
    body_left: Option<f64>,
    absolute: bool,
) -> Result<PageColumn, String> {
    let row_units: Vec<_> = column
        .elements
        .iter()
        .filter_map(|e| e.unit.cloned())
        .collect();
    let rows = units::cluster_unit_rows(&row_units);
    let body = Some(blocks::BodyContext {
        body_left: column.bounds.x0,
        body_right: if single {
            column.bounds.x1.max(page_width - column.bounds.x0)
        } else {
            column.bounds.x1
        },
    });
    let options = blocks::BlockOptions {
        pin_open_leaded_breaks: landscape,
    };
    let mut convert = |lines: &[Line]| -> Result<Vec<PageBlock>, String> {
        let text = blocks::group_into_blocks(lines, body, options)?;
        let text = lists::detect_list_blocks(text, seq, body_left);
        Ok(toc::detect_toc_blocks(text)
            .into_iter()
            .map(PageBlock::Text)
            .collect())
    };
    let text = if absolute {
        let mut lines = Vec::new();
        for row in &rows {
            for run in unit_runs(&row.units) {
                lines.extend(chars::analyze_chars(&run));
            }
        }
        let mut output = Vec::new();
        for stack in stack_lines(lines) {
            output.extend(convert(&stack)?);
        }
        output
    } else {
        let glyphs: Vec<_> = rows
            .iter()
            .flat_map(|r| &r.units)
            .flat_map(|u| u.chars.iter().cloned())
            .collect();
        convert(&chars::analyze_chars(&glyphs))?
    };
    let other = column
        .elements
        .iter()
        .filter_map(|e| e.block.cloned())
        .collect();
    Ok(PageColumn {
        bounds: column.bounds,
        blocks: merge_blocks(text, other),
    })
}
pub(super) fn place_float(sections: &mut [PageSection], img: ImageBlock) {
    if sections.is_empty() {
        return;
    }
    let cy = img.bounds.center_y();
    let si = sections
        .iter()
        .position(|s| cy <= s.bounds.y1 && cy >= s.bounds.y0)
        .unwrap_or(sections.len() - 1);
    let columns = &mut sections[si].columns;
    if columns.is_empty() {
        return;
    }
    let cx = img.bounds.center_x();
    let ci = columns
        .iter()
        .position(|c| cx >= c.bounds.x0 && cx <= c.bounds.x1)
        .unwrap_or_else(|| {
            (0..columns.len())
                .max_by(|&a, &b| {
                    let score = |i: usize| {
                        columns[i].bounds.x1.min(img.bounds.x1)
                            - columns[i].bounds.x0.max(img.bounds.x0)
                    };
                    score(a).total_cmp(&score(b))
                })
                .unwrap()
        });
    let blocks = &mut columns[ci].blocks;
    let i = blocks
        .iter()
        .position(|b| bounds(b).y1 < img.bounds.y1)
        .unwrap_or(blocks.len());
    blocks.insert(i, PageBlock::Image(img));
}
