use jscompat::JsNum;
mod chart_ops;
mod clipboard_ops;
pub(super) mod ipc_de;
mod media_ops;
mod shape_ops;
use super::registry::{OpContext, SlideTarget, field, op_name, record, resolve_slide};
use pptx_engine::insert::{CellAnchor, MAX_INSERT_TABLE_DIM, NewTableCellProps, NewTableOptions};
use pptx_engine::types::EmuRect;
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;

fn number(v: Option<&IpcValue>) -> Option<f64> {
    if let Some(IpcValue::Number(n)) = v {
        Some(*n)
    } else {
        None
    }
}
fn rect(op: &IpcValue) -> Result<EmuRect, String> {
    let b = field(op, "offset");
    let get = |k| b.and_then(|b| number(field(b, k)));
    let (Some(x), Some(y), Some(cx), Some(cy)) = (get("x"), get("y"), get("cx"), get("cy")) else {
        return Err(format!(
            "op \"{}\" needs \"offset\": an EMU rect {{x, y, cx, cy}}.",
            op_name(op)
        ));
    };
    for (key, n) in [("x", x), ("y", y), ("cx", cx), ("cy", cy)] {
        if !n.is_finite() {
            return Err(format!(
                "op \"{}\": \"offset.{key}\" must be a finite number.",
                op_name(op)
            ));
        }
    }
    if cx < 0.0 || cy < 0.0 {
        return Err(format!(
            "op \"{}\": \"offset.cx/cy\" must be >= 0 (EMU).",
            op_name(op)
        ));
    }
    Ok(EmuRect {
        x: JsNum(x),
        y: JsNum(y),
        cx: JsNum(cx),
        cy: JsNum(cy),
    })
}
fn dimensions(op: &IpcValue) -> Result<(f64, f64), String> {
    let rows = number(field(op, "rows"));
    let cols = number(field(op, "cols"));
    let valid =
        |n: f64| n.is_finite() && n.fract() == 0.0 && (1.0..=MAX_INSERT_TABLE_DIM).contains(&n);
    match (rows, cols) {
        (Some(r), Some(c)) if valid(r) && valid(c) => Ok((r, c)),
        _ => Err(format!(
            "op \"addTable\" needs integer \"rows\" and \"cols\" (1..{}).",
            MAX_INSERT_TABLE_DIM
        )),
    }
}
fn sizes(op: &IpcValue, key: &str, count: f64, unit: &str) -> Result<Option<Vec<JsNum>>, String> {
    match field(op, key) {
        None | Some(IpcValue::Undefined) => Ok(None),
        Some(IpcValue::Array(items))
            if items.len() == count as usize
                && items
                    .iter()
                    .all(|v| number(Some(v)).is_some_and(|n| n.is_finite() && n > 0.0)) =>
        {
            Ok(Some(
                items
                    .iter()
                    .filter_map(|v| number(Some(v)).map(JsNum))
                    .collect(),
            ))
        }
        _ => Err(format!(
            "op \"addTable\": \"{key}\" must list exactly {count} positive EMU values (one per {unit})."
        )),
    }
}
fn props(op: &IpcValue) -> Result<Option<Vec<Vec<Option<NewTableCellProps>>>>, String> {
    let Some(value) = field(op, "cellProps") else {
        return Ok(None);
    };
    if matches!(value, IpcValue::Undefined) {
        return Ok(None);
    }
    let IpcValue::Array(rows) = value else {
        return Err("op \"addTable\": \"cellProps\" must be a row-major array of per-cell {gridSpan?,rowSpan?,hMerge?,vMerge?,anchor?}.".into());
    };
    Ok(Some(
        rows.iter()
            .map(|row| match row {
                IpcValue::Array(cells) => cells
                    .iter()
                    .map(|cell| {
                        if matches!(cell, IpcValue::Null | IpcValue::Undefined) {
                            return None;
                        }
                        let flag = |key| match field(cell, key) {
                            Some(IpcValue::Bool(b)) => Some(*b),
                            _ => None,
                        };
                        Some(NewTableCellProps {
                            grid_span: number(field(cell, "gridSpan")).map(JsNum),
                            row_span: number(field(cell, "rowSpan")).map(JsNum),
                            h_merge: flag("hMerge"),
                            v_merge: flag("vMerge"),
                            anchor: match field(cell, "anchor") {
                                Some(IpcValue::String(s)) => match s.as_str() {
                                    "t" => Some(CellAnchor::T),
                                    "ctr" => Some(CellAnchor::Ctr),
                                    "b" => Some(CellAnchor::B),
                                    _ => None,
                                },
                                _ => None,
                            },
                        })
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect(),
    ))
}
fn options(op: &IpcValue) -> Result<NewTableOptions, String> {
    let offset = rect(op)?;
    let (rows, cols) = dimensions(op)?;
    Ok(NewTableOptions {
        rows: JsNum(rows),
        cols: JsNum(cols),
        offset,
        col_widths_emu: sizes(op, "colWidthsEmu", cols, "column")?,
        row_heights_emu: sizes(op, "rowHeightsEmu", rows, "row")?,
        cell_props: props(op)?,
    })
}
pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    if op_name(op) != "addTable" {
        return media_ops::validate(op, ctx)
            .or_else(|| chart_ops::validate(op, ctx))
            .or_else(|| clipboard_ops::validate(op, ctx))
            .or_else(|| shape_ops::validate(op, ctx));
    }
    Some((|| {
        resolve_slide(ctx, op, false)?;
        options(op)?;
        Ok(())
    })())
}
pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    if op_name(op) != "addTable" {
        return media_ops::apply(op, ctx)
            .or_else(|| chart_ops::apply(op, ctx))
            .or_else(|| clipboard_ops::apply(op, ctx))
            .or_else(|| shape_ops::apply(op, ctx));
    }
    Some((|| {
        let SlideTarget::Slide(index) = resolve_slide(ctx, op, false)? else {
            unreachable!()
        };
        let opts = options(op)?;
        let id = pptx_engine::deck::add_table(ctx.opened, index, &opts)?
            .ok_or_else(|| "op \"addTable\": the table could not be inserted.".to_owned())?;
        let mut result = record(op);
        result.created = Some(vec![IpcValue::String(id)]);
        Ok(result)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn op(fields: Vec<(&str, IpcValue)>) -> IpcValue {
        let mut values = vec![("op".into(), IpcValue::String("addTable".into()))];
        values.extend(fields.into_iter().map(|(key, value)| (key.into(), value)));
        IpcValue::Object(values)
    }
    #[test]
    fn table_dimensions_and_cell_merges_are_preserved() {
        let rect = IpcValue::Object(vec![
            ("x".into(), IpcValue::Number(0.0)),
            ("y".into(), IpcValue::Number(0.0)),
            ("cx".into(), IpcValue::Number(100.0)),
            ("cy".into(), IpcValue::Number(100.0)),
        ]);
        let input = op(vec![
            ("offset", rect),
            ("rows", IpcValue::Number(1.0)),
            ("cols", IpcValue::Number(2.0)),
            (
                "colWidthsEmu",
                IpcValue::Array(vec![IpcValue::Number(40.0), IpcValue::Number(60.0)]),
            ),
            (
                "cellProps",
                IpcValue::Array(vec![IpcValue::Array(vec![
                    IpcValue::Object(vec![("gridSpan".into(), IpcValue::Number(2.0))]),
                    IpcValue::Object(vec![("hMerge".into(), IpcValue::Bool(true))]),
                ])]),
            ),
        ]);
        let opts = options(&input).unwrap();
        assert_eq!(
            opts.col_widths_emu
                .unwrap()
                .iter()
                .map(|v| v.0)
                .collect::<Vec<_>>(),
            vec![40.0, 60.0]
        );
        let cells = opts.cell_props.unwrap();
        assert_eq!(cells[0][0].as_ref().unwrap().grid_span.unwrap().0, 2.0);
        assert_eq!(cells[0][1].as_ref().unwrap().h_merge, Some(true));
    }
    #[test]
    fn invalid_grid_width_reports_source_field() {
        let input = op(vec![(
            "colWidthsEmu",
            IpcValue::Array(vec![IpcValue::Number(f64::INFINITY)]),
        )]);
        assert!(
            sizes(&input, "colWidthsEmu", 1.0, "column")
                .unwrap_err()
                .contains("colWidthsEmu")
        );
    }
    #[test]
    fn inserted_table_xml_preserves_grid_spans() {
        let rect = IpcValue::Object(vec![
            ("x".into(), IpcValue::Number(0.0)),
            ("y".into(), IpcValue::Number(0.0)),
            ("cx".into(), IpcValue::Number(100.0)),
            ("cy".into(), IpcValue::Number(100.0)),
        ]);
        let input = op(vec![
            ("offset", rect),
            ("rows", IpcValue::Number(1.0)),
            ("cols", IpcValue::Number(2.0)),
            (
                "colWidthsEmu",
                IpcValue::Array(vec![IpcValue::Number(40.0), IpcValue::Number(60.0)]),
            ),
            (
                "cellProps",
                IpcValue::Array(vec![IpcValue::Array(vec![
                    IpcValue::Object(vec![("gridSpan".into(), IpcValue::Number(2.0))]),
                    IpcValue::Object(vec![("hMerge".into(), IpcValue::Bool(true))]),
                ])]),
            ),
        ]);
        let slide = pptx_engine::types::Slide::default();
        let xml = pptx_engine::insert::build_table_xml(
            &slide,
            &options(&input).unwrap(),
            "00000000-0000-4000-8000-000000000000",
        );
        assert!(xml.contains("gridSpan=\"2\""));
        assert!(xml.contains("hMerge=\"1\""));
        assert!(xml.contains("w=\"40\""));
        assert!(xml.contains("w=\"60\""));
    }
}
