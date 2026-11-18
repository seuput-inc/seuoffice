//! Cross-operation constraints applied before any workbook entry is changed.
use std::collections::HashSet;
use serde_json::Value;

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(format!("{key} must be an array")),
    }
}
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> { value.get(key).and_then(Value::as_str) }
fn shifts_sheet(ops: &[Value], names: &HashSet<&str>) -> bool {
    ops.iter().any(|sheet| text(sheet, "sheetName").is_some_and(|name| names.contains(name))
        && sheet.get("ops").and_then(Value::as_array).is_some_and(|ops| !ops.is_empty()))
}

pub(super) fn validate(request: &Value) -> Result<(), String> {
    let structural = array(request, "structuralOps")?;
    let pivots = array(request, "pivotAdditions")?;
    let sheet_plan = request.get("sheetPlan");
    if !pivots.is_empty() {
        if sheet_plan.is_some() {
            return Err("A new pivot cannot be saved together with sheet management changes — save the pivot first.".into());
        }
        let names: HashSet<_> = pivots.iter().flat_map(|pivot| [text(pivot, "sheetName"), text(pivot, "sourceSheetName")]).flatten().collect();
        if shifts_sheet(structural, &names) {
            return Err("A new pivot cannot be saved together with row/column changes on its sheets — save the pivot first.".into());
        }
    }
    if request.get("slicerState").is_some_and(|state| !state.is_null()) && sheet_plan.is_some() {
        return Err("Slicer changes cannot be saved together with sheet management changes — save the slicers first.".into());
    }
    let tables = array(request, "tableAdditions")?;
    let names: HashSet<_> = tables.iter().filter_map(|table| text(table, "sheetName")).collect();
    if shifts_sheet(structural, &names) {
        return Err("A new table cannot be saved together with row/column changes on its sheet — save the table first.".into());
    }
    let edits = array(request, "tableEdits")?;
    let names: HashSet<_> = edits.iter().filter(|edit| edit.get("area").is_some()).filter_map(|edit| text(edit, "sheetName")).collect();
    if shifts_sheet(structural, &names) {
        return Err("A resized table cannot be saved together with row/column changes on its sheet — save the table first.".into());
    }
    if request.get("definedNamesState").is_some_and(|state| !state.is_null())
        && (sheet_plan.is_some() || structural.iter().any(|sheet| sheet.get("ops").and_then(Value::as_array).is_some_and(|ops| !ops.is_empty()))) {
        return Err("Defined-name edits cannot be saved together with row/column or sheet changes — save one of them first.".into());
    }
    if let Some(plan) = sheet_plan {
        let renames = array(plan, "renames")?;
        let additions = array(plan, "additions")?;
        let hidden = array(plan, "hiddenChanges")?;
        let renamed: HashSet<_> = renames.iter().filter_map(|rename| {
            let old = text(rename, "sheetName")?;
            (text(rename, "newName") != Some(old)).then_some(old)
        }).collect();
        let fields = ["edits", "structuralOps", "filterStates", "hyperlinkEdits", "cfStates", "dvStates", "sheetProtections", "visualAdditions", "pageSetupStates", "noteStates", "tableAdditions", "pivotAdditions", "sparklineAdditions", "formulaValues", "protectedRangeStates", "bulkConstantFills", "tabColorStates", "tableEdits", "cellImageAdditions"];
        let mut addressed = HashSet::new();
        for field in fields {
            addressed.extend(array(request, field)?.iter().filter_map(|state| text(state, "sheetName")));
        }
        addressed.extend(hidden.iter().filter_map(|state| text(state, "sheetName")));
        addressed.extend(additions.iter().filter_map(|state| text(state, "sourceSheetName")));
        for addition in additions {
            if let Some(name) = text(addition, "name") && renamed.contains(name) && addressed.contains(name) {
                return Err(format!("Sheet \"{name}\" is renamed while a new sheet takes its old name in the same save — save with the new sheet under another name first, then rename it."));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn refuses_shifting_a_new_pivot_source() {
        let request = json!({"pivotAdditions":[{"sheetName":"Output","sourceSheetName":"Data"}],"structuralOps":[{"sheetName":"Data","ops":[{"kind":"insertRows"}]}]});
        assert!(validate(&request).unwrap_err().contains("new pivot"));
    }
    #[test]
    fn renamed_sheet_may_not_share_old_name_with_addressed_addition() {
        let request = json!({"sheetPlan":{"renames":[{"sheetName":"Old","newName":"New"}],"additions":[{"name":"Old"}]},"edits":[{"sheetName":"Old"}]});
        assert!(validate(&request).unwrap_err().contains("renamed"));
    }
}
