use pptx_engine::deck::tables::{self, TableMergeOp, TableStructureOp};
mod chart_edit;
mod style_edit;
use pptx_engine::types::{ElementType, TextAnchor};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;
use super::registry::{field, op_name, record, resolve_element, string, OpContext, SlideTarget};

fn number(op: &IpcValue, key: &str) -> Option<f64> { match field(op, key) { Some(IpcValue::Number(n)) => Some(*n), _ => None } }
fn resolve(ctx: &mut OpContext<'_>, op: &IpcValue) -> Result<(SlideTarget, String), String> {
    let (target, index) = resolve_element(ctx, op, false)?;
    let el = &ctx.slide(&target).elements.as_ref().expect("resolved elements")[index];
    if el.element_type() != Some(ElementType::Table) {
        let kind = match el.element_type() { Some(ElementType::Text) => "text", Some(ElementType::Shape) => "shape", Some(ElementType::Picture) => "picture", Some(ElementType::Group) => "group", Some(ElementType::Chart) => "chart", Some(ElementType::Passthrough) => "passthrough", _ => "undefined" };
        return Err(format!("op \"{}\" targets a {kind} element — it only works on table elements.", op_name(op)));
    }
    Ok((target, el.id().to_owned()))
}
fn known(name: &str) -> bool { matches!(name, "setTableCell" | "tableMerge" | "tableStructure" | "setTableRowHeight" | "setTableColWidth" | "setTableCellAnchor") }
fn validate_fields(op: &IpcValue) -> Result<(), String> {
    let name = op_name(op);
    match name {
        "setTableCell" if number(op, "row").is_none() || number(op, "col").is_none() || !matches!(field(op, "paragraphs"), Some(IpcValue::Array(_))) => Err("op \"setTableCell\" needs \"row\", \"col\" and \"paragraphs\".".into()),
        "tableMerge" if field(op, "kind").and_then(string).is_none() || number(op, "row").is_none() || number(op, "col").is_none() => Err("op \"tableMerge\" needs \"kind\", \"row\" and \"col\".".into()),
        "tableStructure" if field(op, "kind").and_then(string).is_none() || number(op, "index").is_none() => Err("op \"tableStructure\" needs \"kind\" and \"index\".".into()),
        "setTableRowHeight" if number(op, "row").is_none() || number(op, "hEmu").is_none() => Err("op \"setTableRowHeight\" needs \"row\" and \"hEmu\".".into()),
        "setTableColWidth" if number(op, "col").is_none() || number(op, "wEmu").is_none() => Err("op \"setTableColWidth\" needs \"col\" and \"wEmu\".".into()),
        "setTableCellAnchor" if number(op, "row").is_none() || number(op, "col").is_none() => Err("op \"setTableCellAnchor\" needs \"row\" and \"col\".".into()),
        "setTableCellAnchor" if !matches!(field(op, "anchor").and_then(string), Some("top" | "middle" | "bottom")) => Err("op \"setTableCellAnchor\" needs \"anchor\": top/middle/bottom.".into()),
        _ => Ok(())
    }
}
pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    if !known(op_name(op)) { return chart_edit::validate(op, ctx).or_else(|| style_edit::validate(op, ctx)); }
    Some((|| { resolve(ctx, op)?; validate_fields(op) })())
}
pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    if !known(op_name(op)) { return chart_edit::apply(op, ctx).or_else(|| style_edit::apply(op, ctx)); }
    Some((|| {
        let (target, id) = resolve(ctx, op)?; validate_fields(op)?;
        let mut result = record(op);
        let n = |key| number(op, key).unwrap_or(f64::NAN);
        match op_name(op) {
            "setTableRowHeight" => if !tables::set_table_row_height(ctx.slide_mut(&target), &id, n("row"), n("hEmu")) { return Err(format!("op \"setTableRowHeight\": row {} does not exist on \"{id}\".", n("row"))); },
            "setTableColWidth" => if !tables::set_table_col_width(ctx.slide_mut(&target), &id, n("col"), n("wEmu")) { return Err(format!("op \"setTableColWidth\": column {} does not exist on \"{id}\".", n("col"))); },
            "setTableCell" => {
                let edited = match field(op, "paragraphs") { Some(IpcValue::Array(v)) => v.as_slice(), _ => &[] };
                let index = |n: f64| if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n < usize::MAX as f64 { Some(n as usize) } else { None };
                let old = ctx.slide(&target).elements.as_ref().and_then(|elements| elements.iter().find(|el| el.id() == id)).and_then(|el| match el { pptx_engine::types::SlideElement::Table(table) => table.rows.as_ref().and_then(|rows| rows.get(index(n("row"))?)).and_then(|row| row.get(index(n("col"))?)).and_then(|cell| cell.text.as_ref()).and_then(|text| text.paragraphs.as_ref()), _ => None });
                let mut paragraphs = super::edit_text::apply_edit_paragraphs(old.map_or(&[], Vec::as_slice), edited);
                for (index, patch) in super::edit_text::collect_paragraph_format_patches(edited) {
                    if let Some(paragraph) = paragraphs.get_mut(index) {
                        pptx_engine::deck::text::apply_paragraph_format(std::slice::from_mut(paragraph), &patch.patch);
                    }
                }
                if !tables::edit_table_cell_text(ctx.slide_mut(&target), &id, n("row"), n("col"), &paragraphs) { return Err(format!("op \"setTableCell\": cell ({}, {}) does not exist on table \"{id}\".", n("row"), n("col"))); }
            }
            "setTableCellAnchor" => { let anchor = match field(op, "anchor").and_then(string) { Some("top") => TextAnchor::Top, Some("middle") => TextAnchor::Middle, _ => TextAnchor::Bottom }; if !tables::set_table_cell_anchor(ctx.slide_mut(&target), &id, n("row"), n("col"), anchor) { return Err(format!("op \"setTableCellAnchor\": cell ({}, {}) does not exist on \"{id}\".", n("row"), n("col"))); } },
            "tableMerge" => {
                let kind = field(op, "kind").and_then(string).unwrap_or(""); let row = n("row"); let col = n("col");
                let operation = match kind { "merge-right" => Some(TableMergeOp::MergeRight { row, col }), "merge-down" => Some(TableMergeOp::MergeDown { row, col }), "split" => Some(TableMergeOp::Split { row, col }), _ => None };
                let SlideTarget::Slide(index) = target else { unreachable!() };
                let changed = if let Some(operation) = operation { tables::merge_table_cells(ctx.opened, index, &id, &operation)? } else { None };
                let changed = changed.ok_or_else(|| format!("op \"tableMerge\": {kind} is not possible at ({row}, {col}) — check merge boundaries."))?;
                result.after = IpcValue::Object(vec![("elementId".into(), IpcValue::String(changed))]);
            }
            "tableStructure" => {
                let kind = field(op, "kind").and_then(string).unwrap_or(""); let ix = n("index"); let before = matches!(field(op, "before"), Some(IpcValue::Bool(true))).then_some(true);
                let operation = match kind { "insert-row" => Some(TableStructureOp::InsertRow { index: ix, before }), "delete-row" => Some(TableStructureOp::DeleteRow { index: ix }), "insert-col" => Some(TableStructureOp::InsertCol { index: ix, before }), "delete-col" => Some(TableStructureOp::DeleteCol { index: ix }), _ => None };
                let SlideTarget::Slide(index) = target else { unreachable!() };
                let changed = if let Some(operation) = operation { tables::edit_table_structure(ctx.opened, index, &id, &operation)? } else { None };
                let changed = changed.ok_or_else(|| format!("op \"tableStructure\": {kind} at index {ix} failed (merges crossing the boundary?)."))?;
                result.after = IpcValue::Object(vec![("elementId".into(), IpcValue::String(changed))]);
            }
            _ => unreachable!(),
        }
        Ok(result)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merge_and_dimension_validation_keep_original_errors() {
        let merge = IpcValue::Object(vec![("op".into(), IpcValue::String("tableMerge".into())), ("kind".into(), IpcValue::String("merge-right".into())), ("row".into(), IpcValue::Number(0.0))]);
        assert_eq!(validate_fields(&merge).unwrap_err(), "op \"tableMerge\" needs \"kind\", \"row\" and \"col\".");
        let height = IpcValue::Object(vec![("op".into(), IpcValue::String("setTableRowHeight".into())), ("row".into(), IpcValue::Number(1.0))]);
        assert_eq!(validate_fields(&height).unwrap_err(), "op \"setTableRowHeight\" needs \"row\" and \"hEmu\".");
    }
}
