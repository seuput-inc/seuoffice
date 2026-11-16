//! Rebuild analyzed PDF pages as an editable spreadsheet.
use super::ir::{
    Alignment as TextAlign, FurnitureHf, IrPage, PageBlock, SoftEdges, Span, TableBlock, TableCell,
    TextBlock,
};
use super::numbers::{ParsedCell, parse_cell_value};
use super::pptx_text::block_runs;
use super::xlsx_package;
use super::xlsx_rowsplit::split_bands;
use super::xlsx_sheet::{Cell, PageSetup, Sheet, Value, cell_ref};
use super::xlsx_styles::{Alignment, Edges, Font, Style, StylePool};
use std::collections::HashMap;

fn flatten(block: &TextBlock) -> String {
    let mut text = block_runs(block, 1.0, false)
        .iter()
        .filter_map(|r| r.text.as_deref())
        .collect::<String>();
    if !text.is_empty() {
        if let Some(list) = &block.list {
            text = format!(
                "{} {text}",
                list.marker.trim_end_matches(jscompat::is_js_space)
            );
        }
        if let Some(toc) = &block.toc_entry {
            text.push_str(&format!("\t{}", toc.page_number));
        }
    }
    text
}
fn font(span: &Span) -> Font {
    Font {
        bold: span.bold,
        italic: span.italic,
        color: (!span.color.is_empty() && span.color != "000000").then(|| span.color.clone()),
        size_pt: (span.font_size > 0.0).then(|| jscompat::math_round(span.font_size * 2.0) / 2.0),
        name: (!span.font_family.is_empty()).then(|| span.font_family.clone()),
    }
}
fn dominant(blocks: &[TextBlock]) -> Option<Font> {
    let mut seen: Vec<(Font, usize)> = Vec::new();
    for b in blocks {
        for l in &b.lines {
            for s in &l.spans {
                if s.invisible || s.text.is_empty() {
                    continue;
                }
                let f = font(s);
                let count = s.text.encode_utf16().count();
                if let Some((_, n)) = seen.iter_mut().find(|(v, _)| *v == f) {
                    *n += count;
                } else {
                    seen.push((f, count));
                }
            }
        }
    }
    let mut best = None;
    let mut weight = 0;
    for (f, n) in seen {
        if n > weight {
            weight = n;
            best = Some(f);
        }
    }
    best.filter(|f| *f != Font::default())
}
fn starts(row: &[TableCell]) -> Vec<usize> {
    let mut n = 0;
    row.iter()
        .map(|c| {
            let v = n;
            n += c.grid_span.max(1);
            v
        })
        .collect()
}
fn bounds(t: &TableBlock) -> Vec<f64> {
    let mut b = vec![t.bounds.x0];
    for w in &t.col_widths_pt {
        b.push(b.last().copied().unwrap_or(0.0) + w);
    }
    b
}
fn cell_text(c: &TableCell) -> String {
    c.blocks.iter().map(flatten).collect::<Vec<_>>().join("\n")
}
fn header(t: &TableBlock) -> Vec<String> {
    t.rows.first().map_or(Vec::new(), |row| {
        row.iter()
            .map(|c| {
                cell_text(c)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    })
}
fn first(page: &IrPage) -> Option<&TableBlock> {
    page.blocks.iter().find_map(|b| {
        if let PageBlock::Table(t) = b {
            Some(t)
        } else {
            None
        }
    })
}
fn last(page: &IrPage) -> Option<&TableBlock> {
    page.blocks.iter().rev().find_map(|b| {
        if let PageBlock::Table(t) = b {
            Some(t)
        } else {
            None
        }
    })
}
fn continues(a: &TableBlock, b: &TableBlock) -> bool {
    if a.col_widths_pt.len() != b.col_widths_pt.len()
        || (a.bounds.x0 - b.bounds.x0).abs() > 2.0
        || (a.bounds.x1 - b.bounds.x1).abs() > 2.0
        || a.col_widths_pt
            .iter()
            .zip(&b.col_widths_pt)
            .any(|(x, y)| (x - y).abs() > 2.0)
    {
        return false;
    }
    let h = header(a);
    let other = header(b);
    h.iter().filter(|s| !s.is_empty()).count() >= 2 && h == other
}
fn groups(pages: &[IrPage]) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    for i in 0..pages.len() {
        if i > 0
            && !pages[i - 1].scanned
            && !pages[i - 1].degraded
            && !pages[i].scanned
            && !pages[i].degraded
            && last(&pages[i - 1])
                .zip(first(&pages[i]))
                .is_some_and(|(a, b)| continues(a, b))
        {
            if let Some(group) = out.last_mut() {
                group.push(i);
                continue;
            }
        }
        out.push(vec![i]);
    }
    out
}
fn border(t: &TableBlock, start: usize, span: usize, soft: Option<&SoftEdges>) -> Option<Edges> {
    let color = t.border_color.clone().unwrap_or_else(|| "000000".into());
    if t.confidence.is_none() {
        let e = soft.cloned().unwrap_or_default();
        return Some(Edges {
            left: (!e.left).then(|| color.clone()),
            right: (!e.right).then(|| color.clone()),
            top: (!e.top).then(|| color.clone()),
            bottom: (!e.bottom).then_some(color),
        });
    }
    if t.separator_rule.is_some() {
        let left = (start > 0).then(|| color.clone());
        let right = (start + span < t.col_widths_pt.len()).then_some(color);
        if left.is_some() || right.is_some() {
            return Some(Edges {
                left,
                right,
                ..Edges::default()
            });
        }
    }
    None
}
fn parsed(text: &str) -> (Value, Option<String>) {
    match parse_cell_value(text) {
        ParsedCell::Number {
            value,
            number_format,
        } => (
            Value::Number(value),
            (number_format != "General").then_some(number_format),
        ),
        ParsedCell::Text(t) => (Value::Text(t.to_owned()), None),
    }
}
struct Builder {
    sheet: Sheet,
    row: usize,
    slots: Vec<f64>,
    widths: Vec<f64>,
}
impl Builder {
    fn slot(&self, x: f64) -> usize {
        self.slots
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| (*a - x).abs().total_cmp(&(*b - x).abs()))
            .map_or(0, |(i, _)| i)
    }
    fn covered(
        &mut self,
        styles: &mut StylePool,
        row: usize,
        col: usize,
        count: usize,
        fill: Option<String>,
        border: Option<Edges>,
    ) {
        let style = styles.cell_xf(&Style {
            fill,
            border,
            ..Style::default()
        });
        for i in 0..count {
            self.sheet.cells.push(Cell {
                row,
                col: col + i,
                value: None,
                style,
            });
        }
    }
    fn text(&mut self, styles: &mut StylePool, b: &TextBlock) {
        let text = flatten(b);
        if jscompat::trim(&text).is_empty() {
            return;
        }
        let (value, number_format) = parsed(&text);
        let style = Style {
            number_format,
            font: dominant(std::slice::from_ref(b)),
            align: text.contains('\n').then(|| Alignment {
                wrap: true,
                ..Alignment::default()
            }),
            ..Style::default()
        };
        self.sheet.cells.push(Cell {
            row: self.row,
            col: self.slot(b.bounds.x0),
            value: Some(value),
            style: styles.cell_xf(&style),
        });
        self.row += 1;
    }
    fn table(&mut self, styles: &mut StylePool, t: &TableBlock, drop_header: bool, grid: &[f64]) {
        let start_row = self.row;
        let skip = usize::from(drop_header);
        let tops: Vec<f64> = t
            .rows
            .iter()
            .enumerate()
            .map(|(r, row)| {
                row.iter()
                    .find(|c| c.v_merge.as_deref() != Some("continue"))
                    .or_else(|| row.first())
                    .map_or(t.bounds.y1 - r as f64, |c| c.bounds.y1)
            })
            .collect();
        for (i, w) in t.col_widths_pt.iter().enumerate() {
            let col = self.slot(grid[i]);
            if self.widths.len() <= col {
                self.widths.resize(col + 1, 0.0);
            }
            self.widths[col] = self.widths[col].max(*w);
        }
        for (r, row) in t.rows.iter().enumerate().skip(skip) {
            let sheet_row = start_row + r - skip;
            let bottom = tops.get(r + 1).copied().unwrap_or(t.bounds.y0);
            self.sheet
                .heights
                .insert(sheet_row, (tops[r] - bottom).clamp(1.0, 409.0));
            let positions = starts(row);
            for (i, c) in row.iter().enumerate() {
                let col_start = positions[i];
                let span = c.grid_span.max(1);
                let edge = border(t, col_start, span, c.soft_edges.as_ref());
                let col = self.slot(grid[col_start]);
                let end = col.max(self.slot(grid[col_start + span]).saturating_sub(1));
                let width = end - col + 1;
                if c.v_merge.as_deref() == Some("continue") {
                    self.covered(styles, sheet_row, col, width, c.fill.clone(), edge);
                    continue;
                }
                let mut rows = 1;
                if c.v_merge.as_deref() == Some("restart") {
                    for subsequent in t.rows.iter().skip(r + 1) {
                        let indices = starts(subsequent);
                        if indices
                            .iter()
                            .position(|v| *v == col_start)
                            .is_some_and(|idx| {
                                subsequent[idx].v_merge.as_deref() == Some("continue")
                            })
                        {
                            rows += 1;
                        } else {
                            break;
                        }
                    }
                }
                if width > 1 || rows > 1 {
                    self.sheet.merges.push(format!(
                        "{}:{}",
                        cell_ref(sheet_row, col),
                        cell_ref(sheet_row + rows - 1, end)
                    ));
                }
                let text = cell_text(c);
                let (value, number_format) = parsed(&text);
                let horizontal = if matches!(&value, Value::Text(_)) {
                    c.blocks.first().and_then(|b| match b.align {
                        TextAlign::Center => Some("center".into()),
                        TextAlign::Right => Some("right".into()),
                        _ => None,
                    })
                } else {
                    None
                };
                let alignment = Alignment {
                    vertical: Some(c.v_align.clone().unwrap_or_else(|| "top".into())),
                    horizontal,
                    wrap: text.contains('\n'),
                };
                let style = Style {
                    number_format,
                    font: dominant(&c.blocks),
                    fill: c.fill.clone(),
                    border: edge.clone(),
                    align: Some(alignment),
                };
                self.sheet.cells.push(Cell {
                    row: sheet_row,
                    col,
                    value: (!text.is_empty()).then_some(value),
                    style: styles.cell_xf(&style),
                });
                self.covered(styles, sheet_row, col + 1, width - 1, c.fill.clone(), edge);
            }
        }
        self.row = start_row + t.rows.len() - skip;
    }
}

fn emit_group(pages: &[&IrPage], styles: &mut StylePool) -> Sheet {
    let first_page = pages[0];
    let last_page = pages[pages.len() - 1];
    let name = if pages.len() == 1 {
        format!("Page {}", first_page.index + 1)
    } else {
        format!("Pages {}-{}", first_page.index + 1, last_page.index + 1)
    };
    let mut run_bounds: HashMap<*const TableBlock, Vec<f64>> = HashMap::new();
    for i in 1..pages.len() {
        if let (Some(previous), Some(next)) = (last(pages[i - 1]), first(pages[i])) {
            let anchor = run_bounds
                .get(&(previous as *const TableBlock))
                .cloned()
                .unwrap_or_else(|| bounds(previous));
            run_bounds.insert(next as *const TableBlock, anchor);
        }
    }
    let mut xs = Vec::new();
    for page in pages {
        for block in &page.blocks {
            match block {
                PageBlock::Image(_) => {}
                PageBlock::Text(t) => xs.push(t.bounds.x0),
                PageBlock::Table(t) => xs.extend(
                    run_bounds
                        .get(&(t as *const TableBlock))
                        .cloned()
                        .unwrap_or_else(|| bounds(t)),
                ),
            }
        }
        for note in &page.footnotes {
            for block in &note.blocks {
                xs.push(block.bounds.x0);
            }
        }
    }
    xs.sort_by(f64::total_cmp);
    let mut slots = Vec::new();
    for x in xs {
        if slots.last().is_none_or(|last| x - *last > 1.5) {
            slots.push(x);
        }
    }
    let mut b = Builder {
        sheet: Sheet {
            name,
            ..Sheet::default()
        },
        row: 0,
        slots,
        widths: Vec::new(),
    };
    if first_page.scanned || first_page.degraded {
        let kind = if first_page.scanned {
            "scanned page".to_owned()
        } else {
            format!(
                "degraded page ({})",
                first_page.degraded_reason.as_deref().unwrap_or("unknown")
            )
        };
        b.sheet.cells.push(Cell {
            row: 0,
            col: 0,
            style: 0,
            value: Some(Value::Text(format!(
                "Page {}: {kind} — not convertible to cells",
                first_page.index + 1
            ))),
        });
        return b.sheet;
    }
    let mut last_table = false;
    for (index, page) in pages.iter().enumerate() {
        let continuation = if index > 0 {
            first(page).map(|t| t as *const TableBlock)
        } else {
            None
        };
        for block in &page.blocks {
            match block {
                PageBlock::Image(_) => {}
                PageBlock::Text(t) => {
                    b.text(styles, t);
                    last_table = false;
                }
                PageBlock::Table(t) => {
                    let ptr = t as *const TableBlock;
                    let cont = continuation == Some(ptr);
                    if !cont && last_table {
                        b.row += 1;
                    }
                    let split = split_bands(t);
                    let grid = run_bounds.get(&ptr).cloned().unwrap_or_else(|| bounds(t));
                    b.table(styles, &split, cont, &grid);
                    last_table = true;
                }
            }
        }
        for note in &page.footnotes {
            for block in &note.blocks {
                b.text(styles, block);
            }
        }
    }
    b.sheet.widths = b
        .widths
        .into_iter()
        .map(|w| {
            if w > 0.0 {
                Some(((w * (4.0 / 3.0) - 5.0) / 7.0).max(2.0))
            } else {
                None
            }
        })
        .collect();
    b.sheet
}
fn header_footer(furniture: &[&FurnitureHf], width: f64) -> (Option<String>, Option<String>) {
    let mut ordered = furniture.to_vec();
    ordered.sort_by(|a, b| {
        if a.band == "top" {
            a.edge_dist_pt.total_cmp(&b.edge_dist_pt)
        } else {
            b.edge_dist_pt.total_cmp(&a.edge_dist_pt)
        }
    });
    let mut bands: [Vec<String>; 6] = Default::default();
    for hf in ordered {
        let cx = (hf.x0 + hf.x1) / 2.0;
        let section = if cx < width / 3.0 {
            0
        } else if cx > 2.0 * width / 3.0 {
            2
        } else {
            1
        };
        let band = if hf.band == "top" { 0 } else { 3 };
        bands[band + section].push(hf.text.replace('&', "&&").replace('\u{e001}', "&P"));
    }
    let encode = |at: usize| {
        let mut s = String::new();
        for (i, key) in ["&L", "&C", "&R"].iter().enumerate() {
            if !bands[at + i].is_empty() {
                s.push_str(key);
                s.push_str(&bands[at + i].join("\n"));
            }
        }
        (!s.is_empty()).then_some(s)
    };
    (encode(0), encode(3))
}
fn paper(page: Option<&IrPage>) -> PageSetup {
    let Some(page) = page else {
        return PageSetup {
            orientation: Some("portrait".into()),
            ..PageSetup::default()
        };
    };
    let w = page.width_pt.min(page.height_pt);
    let h = page.width_pt.max(page.height_pt);
    let sizes = [
        (612.0_f64, 792.0_f64, 1),
        (612.0, 1008.0, 5),
        (842.0, 1191.0, 8),
        (595.0, 842.0, 9),
        (420.0, 595.0, 11),
        (516.0, 729.0, 13),
    ];
    PageSetup {
        paper_size: sizes
            .iter()
            .find(|(a, b, _)| (a - w).abs() <= 3.0 && (b - h).abs() <= 3.0)
            .map(|(_, _, n)| *n),
        orientation: Some(
            if page.width_pt > page.height_pt {
                "landscape"
            } else {
                "portrait"
            }
            .into(),
        ),
        first_page_number: None,
    }
}
pub(super) fn rebuild(
    pages: &[IrPage],
    furniture: &[FurnitureHf],
) -> Result<(Vec<u8>, Vec<String>), String> {
    let mut styles = StylePool::default();
    let groups = groups(pages);
    let mut sheets: Vec<_> = groups
        .iter()
        .map(|g| {
            emit_group(
                &g.iter().map(|i| &pages[*i]).collect::<Vec<_>>(),
                &mut styles,
            )
        })
        .collect();
    if sheets.is_empty() {
        sheets.push(Sheet {
            name: "Page 1".into(),
            ..Sheet::default()
        });
    }
    let width = pages.first().map_or(612.0, |p| p.width_pt);
    let all: Vec<_> = furniture.iter().collect();
    let first: Vec<_> = furniture.iter().filter(|f| f.covers_first_page).collect();
    let full = header_footer(&all, width);
    let initial = header_footer(&first, width);
    if full.0.is_some() || full.1.is_some() {
        let numbered = furniture.iter().any(|f| f.page_no);
        for (i, sheet) in sheets.iter_mut().enumerate() {
            let page_index = groups.get(i).and_then(|g| g.first()).copied().unwrap_or(0);
            let hf = if page_index == 0 { &initial } else { &full };
            if hf.0.is_none() && hf.1.is_none() {
                continue;
            }
            sheet.header = hf.0.clone();
            sheet.footer = hf.1.clone();
            let mut setup = paper(pages.get(page_index));
            if numbered {
                setup.first_page_number = Some(page_index + 1);
            }
            sheet.setup = Some(setup);
        }
    }
    let mut warnings = Vec::new();
    for sheet in &mut sheets {
        let name = sheet.name.clone();
        warnings.extend(
            sheet
                .normalize()
                .into_iter()
                .map(|w| format!("{name}: {w}")),
        );
    }
    if !pages
        .iter()
        .any(|p| p.blocks.iter().any(|b| matches!(b, PageBlock::Table(_))))
        && pages.iter().any(|p| !p.scanned && !p.degraded)
    {
        warnings.push("no tables detected".into());
    }
    Ok((xlsx_package::build(&sheets, &styles)?, warnings))
}

#[cfg(test)]
mod tests {
    use super::super::geometry::Rect;
    use super::*;
    fn table(x: f64) -> TableBlock {
        TableBlock {
            bounds: Rect {
                x0: x,
                y0: 0.0,
                x1: x + 100.0,
                y1: 20.0,
            },
            col_widths_pt: vec![50.0, 50.0],
            rows: Vec::new(),
            confidence: None,
            border_color: None,
            form: false,
            separator_rule: None,
            spacing_before_pt: None,
        }
    }
    #[test]
    fn table_style_respects_soft_edges_and_separators() {
        let mut grid = table(0.0);
        let edges = border(
            &grid,
            0,
            1,
            Some(&SoftEdges {
                top: true,
                ..SoftEdges::default()
            }),
        )
        .unwrap();
        assert!(edges.top.is_none());
        assert_eq!(edges.right.as_deref(), Some("000000"));
        grid.confidence = Some(0.9);
        grid.separator_rule = Some("vertical".into());
        let edges = border(&grid, 0, 1, None).unwrap();
        assert!(edges.left.is_none());
        assert_eq!(edges.right.as_deref(), Some("000000"));
    }
}
