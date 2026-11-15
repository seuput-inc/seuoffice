//! Checkbox-led form rows with merged continuation descriptions.
use std::collections::HashSet;
use super::super::geometry::{Rect, median};
use super::super::ir::{PageShapes, PdfChar, Stroke, TableBlock, TableCell, TextBlock};
use super::super::lines::space;
use super::super::script::script_of;
use super::table::group_strokes;
use super::units::{LineUnit, UnitRow, cluster_unit_rows};

pub(super) struct DetectedFormTables { pub tables: Vec<TableBlock>, pub remaining_units: Vec<LineUnit> }

pub(super) fn detect_checkbox_squares(strokes: &[Stroke]) -> Vec<Rect> {
    group_strokes(strokes).into_iter().filter_map(|group| {
        let h = group.iter().filter(|&&i| strokes[i].orientation == "h").count();
        let v = group.iter().filter(|&&i| strokes[i].orientation == "v").count();
        if h < 2 || v < 2 || group.len() > 8 { return None; }
        let bounds = group.iter().map(|&i| strokes[i].bounds).reduce(Rect::union)?;
        let (w, h) = (bounds.width(), bounds.height());
        ((5.0..=18.0).contains(&w) && (5.0..=18.0).contains(&h) && (w - h).abs() <= 0.35 * w.max(h)).then_some(bounds)
    }).collect()
}

fn square_of(unit: &LineUnit, squares: &[Rect]) -> Option<Rect> {
    squares.iter().copied().find(|square| {
        let gap = unit.bounds.x0 - square.x1;
        gap >= -1.0 && gap <= 1.2 * unit.font_size && square.vertical_overlap_ratio(unit.bounds) >= 0.5
    })
}
fn checkbox_char(square: Rect, unit: &LineUnit) -> PdfChar {
    let anchor = unit.chars.iter().find(|c| !space(c.code) && c.code > 0x1f).unwrap_or(&unit.chars[0]);
    PdfChar { code: 0x2610, text: "☐".into(), bounds: square, loose_box: square,
        origin_x: square.x0, origin_y: unit.baseline, angle: 0.0, font_size: anchor.font_size,
        font_weight: anchor.font_weight, font_family: anchor.font_family.clone(), italic: false,
        color: anchor.color.clone(), hscale: None, invisible: false, is_generated: true,
        pdf_char_index: None, text_object_id: None, is_hyphen: false, script: script_of(0x2610),
        highlight: None, underline: false, strike: false, note_ref: None, href: None }
}
struct FormRow { row: UnitRow, squares: Vec<Rect>, bounds: Rect }
fn form_row(row: &UnitRow, squares: &[Rect]) -> Option<FormRow> {
    if row.units.len() < 2 { return None; }
    let matches: Vec<_> = row.units.iter().map(|unit| square_of(unit, squares)).collect::<Option<_>>()?;
    let bounds = matches.iter().copied().fold(row.bounds, Rect::union);
    Some(FormRow { row: row.clone(), squares: matches, bounds })
}
fn cell(bounds: Rect, grid_span: usize, blocks: Vec<TextBlock>) -> TableCell {
    TableCell { bounds, grid_span, v_merge: None, fill: None, v_align: None, soft_edges: None, blocks }
}
fn build_table(form: &[FormRow], continuation: &[UnitRow], build_blocks: &impl Fn(&[PdfChar]) -> Result<Vec<TextBlock>, String>) -> Result<TableBlock, String> {
    let cols = form[0].row.units.len();
    let spans: Vec<_> = (0..cols).map(|col| (
        form.iter().map(|row| row.squares[col].x0).fold(f64::INFINITY, f64::min),
        form.iter().map(|row| row.row.units[col].bounds.x1).fold(f64::NEG_INFINITY, f64::max),
    )).collect();
    let bounds = form.iter().map(|row| row.bounds).chain(continuation.iter().map(|row| row.bounds)).reduce(Rect::union).expect("form has rows");
    let mut xs = vec![bounds.x0];
    for col in 1..cols { xs.push((spans[col - 1].1 + spans[col].0) / 2.0); }
    xs.push(bounds.x1);
    let mut rows = Vec::new();
    for row in form {
        let mut cells = Vec::new();
        for (col, unit) in row.row.units.iter().enumerate() {
            let box_ = Rect { x0: xs[col], x1: xs[col + 1], y0: row.bounds.y0, y1: row.bounds.y1 };
            let mut chars = Vec::with_capacity(unit.chars.len() + 1);
            chars.push(checkbox_char(row.squares[col], unit));
            chars.extend(unit.chars.iter().cloned());
            cells.push(cell(box_, 1, build_blocks(&chars)?));
        }
        rows.push(cells);
    }
    if !continuation.is_empty() {
        let continuation_bounds = continuation.iter().map(|row| row.bounds).reduce(Rect::union).expect("nonempty");
        let bounds = Rect { x0: bounds.x0, x1: bounds.x1, y0: continuation_bounds.y0, y1: continuation_bounds.y1 };
        let chars: Vec<_> = continuation.iter().flat_map(|row| row.units.iter()).flat_map(|unit| unit.chars.iter().cloned()).collect();
        rows.push(vec![cell(bounds, cols, build_blocks(&chars)?)]);
    }
    Ok(TableBlock { bounds, col_widths_pt: xs.windows(2).map(|x| x[1] - x[0]).collect(), rows,
        confidence: Some(0.9), border_color: None, form: true, separator_rule: None, spacing_before_pt: None })
}

pub(super) fn detect_form_tables(units: &[LineUnit], shapes: &PageShapes, excluded: &[Rect], build_blocks: &impl Fn(&[PdfChar]) -> Result<Vec<TextBlock>, String>) -> Result<DetectedFormTables, String> {
    let squares: Vec<_> = detect_checkbox_squares(&shapes.strokes).into_iter().filter(|sq| !excluded.iter().any(|b| b.intersection_area(*sq) > 0.0)).collect();
    if squares.is_empty() { return Ok(DetectedFormTables { tables: Vec::new(), remaining_units: units.to_vec() }); }
    let rows = cluster_unit_rows(units);
    let mut tables = Vec::new();
    let mut consumed = HashSet::new();
    let mut i = 0;
    while i < rows.len() {
        let Some(first) = form_row(&rows[i], &squares) else { i += 1; continue; };
        let count = first.row.units.len();
        let mut form = vec![first];
        let mut j = i + 1;
        while j < rows.len() {
            let Some(next) = form_row(&rows[j], &squares) else { break; };
            let previous = form.last().expect("form row");
            if next.row.units.len() != count || previous.bounds.y0 - next.bounds.y1 > 1.6 * previous.bounds.height().max(1.0) { break; }
            form.push(next); j += 1;
        }
        let height = median(&form.iter().map(|row| row.row.bounds.height()).collect::<Vec<_>>());
        let height = if height == 0.0 { 12.0 } else { height };
        let left = form.iter().map(|row| row.bounds.x0).fold(f64::INFINITY, f64::min);
        let mut continuation: Vec<UnitRow> = Vec::new();
        while j < rows.len() && continuation.len() < 4 {
            let row = &rows[j];
            if row.units.len() != 1 { break; }
            let unit = &row.units[0];
            if square_of(unit, &squares).is_some() { break; }
            let above = continuation.last().map(|row| row.bounds).unwrap_or_else(|| form.last().expect("form row").bounds);
            if above.y0 - row.bounds.y1 > 0.75 * height || (unit.bounds.x0 - left).abs() > 2.0 * unit.font_size { break; }
            continuation.push(row.clone()); j += 1;
        }
        tables.push(build_table(&form, &continuation, build_blocks)?);
        consumed.extend(form.iter().flat_map(|row| row.row.units.iter()).chain(continuation.iter().flat_map(|row| row.units.iter())).map(|unit| unit.id));
        i = j;
    }
    tables.sort_by(|a,b| b.bounds.y1.total_cmp(&a.bounds.y1));
    Ok(DetectedFormTables { tables, remaining_units: units.iter().filter(|unit| !consumed.contains(&unit.id)).cloned().collect() })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drawn_checkbox_square_is_detected_but_not_a_long_rule() {
        let stroke = |bounds, orientation: &str| Stroke { bounds, orientation: orientation.into(), width_pt: 0.5, color: "000000".into(), from_form: false };
        let strokes = vec![
            stroke(Rect { x0:0.0,y0:0.0,x1:10.0,y1:0.0 }, "h"),
            stroke(Rect { x0:0.0,y0:10.0,x1:10.0,y1:10.0 }, "h"),
            stroke(Rect { x0:0.0,y0:0.0,x1:0.0,y1:10.0 }, "v"),
            stroke(Rect { x0:10.0,y0:0.0,x1:10.0,y1:10.0 }, "v"),
            stroke(Rect { x0:100.0,y0:0.0,x1:200.0,y1:0.0 }, "h"),
        ];
        assert_eq!(detect_checkbox_squares(&strokes), vec![Rect { x0:0.0,y0:0.0,x1:10.0,y1:10.0 }]);
    }
}
