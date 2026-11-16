//! Rebuild measured PDF tables as native DOCX table XML.

use docx_engine::{generate::table::generate_table_model_xml, model::TableModel};
use jscompat::math_round;
use serde_json::{Map, Value, json};

use super::super::{
    geometry::Rect,
    ir::{Line, TableBlock, TableCell},
};
use super::{
    fit::preflight_fit_block,
    text::{block_format, paragraph_runs},
};

const TWIPS: f64 = 20.0;

fn row_height_twips(row: &[TableCell]) -> Option<f64> {
    row.iter()
        .filter(|cell| cell.v_merge.is_none())
        .map(|cell| cell.bounds.height())
        .reduce(f64::min)
        .map(|height| math_round(height * TWIPS).max(1.0))
}

fn row_boundaries(table: &TableBlock) -> Option<Vec<f64>> {
    let n = table.rows.len();
    if n == 0 {
        return None;
    }
    let mut ys = vec![None; n + 1];
    ys[0] = Some(table.bounds.y1);
    ys[n] = Some(table.bounds.y0);
    for row in 0..n {
        for cell in &table.rows[row] {
            if cell.v_merge.as_deref() == Some("continue") {
                continue;
            }
            if ys[row].is_none() {
                ys[row] = Some(cell.bounds.y1);
            }
            let mut span = 1;
            while row + span < n
                && table.rows[row + span].iter().any(|next| {
                    next.v_merge.as_deref() == Some("continue")
                        && (next.bounds.y0 - cell.bounds.y0).abs() < 0.1
                        && (next.bounds.x0 - cell.bounds.x0).abs() < 0.1
                })
            {
                span += 1;
            }
            if ys[row + span].is_none() {
                ys[row + span] = Some(cell.bounds.y0);
            }
        }
    }
    for i in 1..n {
        if ys[i].is_some() {
            continue;
        }
        let mut hi = i + 1;
        while hi < n && ys[hi].is_none() {
            hi += 1;
        }
        let lo = i - 1;
        let start = ys[lo]?;
        let step = (ys[hi]? - start) / (hi - lo) as f64;
        for (k, slot) in ys.iter_mut().enumerate().take(hi).skip(lo + 1) {
            *slot = Some(start + step * (k - lo) as f64);
        }
    }
    let ys = ys.into_iter().collect::<Option<Vec<_>>>()?;
    if ys.windows(2).any(|pair| pair[1] >= pair[0] - 0.1) {
        return None;
    }
    Some(ys)
}

pub(super) fn row_heights_pt(table: &TableBlock) -> Vec<Option<f64>> {
    let mut heights: Vec<_> = table
        .rows
        .iter()
        .map(|row| row_height_twips(row).map(|h| h / TWIPS))
        .collect();
    if heights.iter().all(Option::is_some) {
        return heights;
    }
    let Some(ys) = row_boundaries(table) else {
        return heights;
    };
    if ys.len() != table.rows.len() + 1 {
        return heights;
    }
    let bounds: Vec<_> = ys
        .windows(2)
        .map(|pair| (pair[0] - pair[1]).max(1.0))
        .collect();
    let height = table.bounds.height();
    if (bounds.iter().sum::<f64>() - height).abs() > 4.0_f64.max(height * 0.02) {
        return heights;
    }
    for (value, boundary) in heights.iter_mut().zip(bounds) {
        if value.is_none() {
            *value = Some(boundary);
        }
    }
    heights
}

fn squeeze(table: &TableBlock, height_scale: f64) -> f64 {
    if table.separator_rule.is_some()
        || table
            .rows
            .iter()
            .flatten()
            .any(|cell| cell.v_merge.is_some())
    {
        return 1.0;
    }
    let box_twips = table.bounds.height() * TWIPS * height_scale;
    let mut emitted = 0.0;
    for row in &table.rows {
        let mut tallest = row_height_twips(row).unwrap_or(0.0) * height_scale;
        for cell in row {
            let height: f64 = cell
                .blocks
                .iter()
                .map(|block| {
                    block_format(block, height_scale)
                        .get("lineRawTwips")
                        .and_then(Value::as_f64)
                        .unwrap_or(40.0)
                        * block.lines.len().max(1) as f64
                })
                .sum();
            tallest = tallest.max(height);
        }
        emitted += tallest;
    }
    if emitted <= box_twips + 60.0_f64.max(box_twips * 0.02) {
        1.0
    } else {
        (box_twips / emitted).max(0.55)
    }
}

fn cell_alignment(bounds: Rect, lines: &[Line]) -> Option<&'static str> {
    if lines.is_empty() {
        return None;
    }
    let width = bounds.width();
    let tolerance = 1.5_f64.max(width * 0.04);
    let center = bounds.center_x();
    let mut widest = 0.0_f64;
    let mut centered = true;
    let mut righted = true;
    for line in lines {
        widest = widest.max(line.bounds.width());
        if (line.bounds.center_x() - center).abs() >= tolerance {
            centered = false;
        }
        if bounds.x1 - line.bounds.x1 >= tolerance || line.bounds.x0 - bounds.x0 < tolerance {
            righted = false;
        }
    }
    if width <= widest * 1.15 {
        None
    } else if centered {
        Some("center")
    } else if righted {
        Some("right")
    } else {
        None
    }
}

fn no_borders() -> Value {
    json!({"top":{"style":"none"},"left":{"style":"none"},"bottom":{"style":"none"},"right":{"style":"none"},"insideH":{"style":"none"},"insideV":{"style":"none"}})
}

fn cell_paragraphs(cell: &TableCell, trailing: bool, scale: f64, infer_align: bool) -> Vec<Value> {
    let mut paragraphs: Vec<Value> = cell
        .blocks
        .iter()
        .map(|block| {
            let mut format = block_format(block, scale);
            if let Some(spacing) = block.spacing_before_pt.filter(|spacing| *spacing > 0.0) {
                let ink = math_round(block.bounds.height() * TWIPS);
                let excess = (format
                    .get("lineRawTwips")
                    .and_then(Value::as_f64)
                    .unwrap_or(ink)
                    - ink)
                    .max(0.0);
                let before = math_round(spacing * TWIPS) - excess;
                if before > 0.0 {
                    format.insert("spaceBefore".into(), json!(before));
                }
            }
            if infer_align && !format.contains_key("align") {
                if let Some(align) = cell_alignment(cell.bounds, &block.lines) {
                    format.insert("align".into(), json!(align));
                }
            }
            format.insert("runs".into(), Value::Array(paragraph_runs(block)));
            Value::Object(format)
        })
        .collect();
    if trailing {
        if let Some(Value::String(text)) = paragraphs
            .last_mut()
            .and_then(|p| p.get_mut("runs"))
            .and_then(Value::as_array_mut)
            .and_then(|runs| runs.last_mut())
            .and_then(|run| run.get_mut("text"))
        {
            if !text.is_empty() && !text.chars().next_back().is_some_and(jscompat::is_js_space) {
                text.push(' ');
            }
        }
    }
    paragraphs
}

pub(super) fn table(
    block: &mut TableBlock,
    height_scale: f64,
    indent_pt: f64,
    shave_pt: f64,
    float_pos: Option<Value>,
    measure: &impl Fn(&str, &str, f64, bool, bool) -> Option<f64>,
) -> Result<Value, String> {
    let row_scale = height_scale * squeeze(block, height_scale);
    let heights = row_heights_pt(block);
    let lattice = block.confidence.is_none();
    let strict = block.separator_rule.is_some();
    let mut model = Map::new();
    model.insert("cellMarTwips".into(), json!({"left":15,"right":15}));
    if let Some(pos) = float_pos {
        model.insert("floatPos".into(), pos);
    }
    if indent_pt >= 2.0 {
        model.insert("indentTwips".into(), json!(math_round(indent_pt * TWIPS)));
    }
    if !lattice {
        let mut borders = no_borders();
        if let Some(rule) = &block.separator_rule {
            borders["insideV"] = json!({"style":rule,"szEighths":4});
        }
        model.insert("borders".into(), borders);
    } else if let Some(color) = &block.border_color {
        let line = json!({"style":"single","szEighths":4,"color":color});
        model.insert("borders".into(), json!({"top":line,"left":line,"bottom":line,"right":line,"insideH":line,"insideV":line}));
    }
    let mut rows = Vec::with_capacity(block.rows.len());
    for row in &mut block.rows {
        let mut start = 0usize;
        let mut cells = Vec::with_capacity(row.len());
        for cell in row {
            let span = cell.grid_span.max(1);
            let end = start.saturating_add(span).min(block.col_widths_pt.len());
            let width: f64 = block
                .col_widths_pt
                .get(start..end)
                .unwrap_or(&[])
                .iter()
                .sum();
            start = start.saturating_add(span);
            for text in &mut cell.blocks {
                preflight_fit_block(text, width - 1.5, strict, measure);
            }
            let paragraphs = cell_paragraphs(cell, block.form, row_scale, lattice);
            let plain: Vec<String> = paragraphs
                .iter()
                .map(|para| {
                    para["runs"]
                        .as_array()
                        .map(|runs| {
                            runs.iter()
                                .filter_map(|run| run["text"].as_str())
                                .collect::<String>()
                        })
                        .unwrap_or_default()
                })
                .collect();
            let mut output = Map::new();
            output.insert(
                "paras".into(),
                if plain.is_empty() {
                    json!([""])
                } else {
                    json!(plain)
                },
            );
            output.insert(
                "richParas".into(),
                if paragraphs.is_empty() {
                    json!([{"spaceAfter":0,"lineRule":"exact","lineRawTwips":20,"runs":[]}])
                } else {
                    Value::Array(paragraphs)
                },
            );
            if cell.grid_span > 1 {
                output.insert("colSpan".into(), json!(cell.grid_span));
            }
            if let Some(merge) = &cell.v_merge {
                output.insert("vMerge".into(), json!(merge));
            }
            if let Some(fill) = &cell.fill {
                output.insert("fill".into(), json!(fill));
            }
            if let Some(align) = &cell.v_align {
                output.insert("vAlign".into(), json!(align));
            }
            if lattice {
                if let Some(edges) = &cell.soft_edges {
                    let mut borders = Map::new();
                    for (key, enabled) in [
                        ("left", edges.left),
                        ("right", edges.right),
                        ("top", edges.top),
                        ("bottom", edges.bottom),
                    ] {
                        if enabled {
                            borders.insert(key.into(), json!({"style":"none"}));
                        }
                    }
                    output.insert("borders".into(), Value::Object(borders));
                }
            }
            cells.push(Value::Object(output));
        }
        rows.push(Value::Array(cells));
    }
    model.insert("rows".into(), Value::Array(rows));
    model.insert(
        "colWidthsTwips".into(),
        Value::Array(
            block
                .col_widths_pt
                .iter()
                .map(|width| json!(math_round(width * TWIPS).max(1.0)))
                .collect(),
        ),
    );
    let shave = math_round(shave_pt * TWIPS / block.rows.len().max(1) as f64);
    model.insert(
        "rowHeightsTwips".into(),
        Value::Array(
            heights
                .into_iter()
                .map(|height| match height {
                    None => Value::Null,
                    Some(height) => json!(
                        (math_round(math_round(height * TWIPS).max(1.0) * row_scale)
                            - if lattice { 10.0 } else { 0.0 }
                            - shave)
                            .max(1.0)
                    ),
                })
                .collect(),
        ),
    );
    let model: TableModel =
        serde_json::from_value(Value::Object(model)).map_err(|error| error.to_string())?;
    let xml = generate_table_model_xml(&model, None, None).map_err(|error| error.to_string())?;
    Ok(json!({"kind":"xml","xml":xml}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged_table() -> TableBlock {
        let bounds = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 100.0,
            y1: 20.0,
        };
        let cell = |merge: &str| TableCell {
            bounds,
            grid_span: 1,
            v_merge: Some(merge.into()),
            fill: None,
            v_align: None,
            soft_edges: None,
            blocks: Vec::new(),
        };
        TableBlock {
            bounds,
            col_widths_pt: vec![100.0],
            rows: vec![vec![cell("restart")], vec![cell("continue")]],
            confidence: None,
            border_color: None,
            form: false,
            separator_rule: None,
            spacing_before_pt: None,
        }
    }

    #[test]
    fn merged_rows_keep_measured_height_and_emit_merge_markup() {
        let mut block = merged_table();
        assert_eq!(row_heights_pt(&block), vec![Some(10.0), Some(10.0)]);
        let result = table(&mut block, 1.0, 4.0, 0.0, None, &|_, _, _, _, _| None).unwrap();
        let xml = result["xml"].as_str().unwrap();
        assert!(xml.contains("<w:vMerge w:val=\"restart\"/>"));
        assert!(xml.contains("<w:vMerge/>"));
        assert_eq!(xml.matches("<w:trHeight w:val=\"190\"").count(), 2);
        assert!(xml.contains("<w:tblInd w:w=\"80\""));
    }

    #[test]
    fn borderless_spanned_cells_keep_fill_and_separator() {
        let mut block = merged_table();
        block.rows.truncate(1);
        block.rows[0][0].v_merge = None;
        block.rows[0][0].grid_span = 2;
        block.rows[0][0].fill = Some("FFDD00".into());
        block.col_widths_pt = vec![40.0, 60.0];
        block.confidence = Some(0.8);
        block.separator_rule = Some("single".into());
        let result = table(&mut block, 1.0, 0.0, 0.0, None, &|_, _, _, _, _| None).unwrap();
        let xml = result["xml"].as_str().unwrap();
        assert!(xml.contains("<w:gridSpan w:val=\"2\""));
        assert!(xml.contains("FFDD00"));
        assert!(xml.contains("<w:insideV w:val=\"single\""));
    }
}
