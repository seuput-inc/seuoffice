//! Drop stale calculation chains after worksheet or sheet-set changes.
use super::package::PackageEditor;
use jscompat::TryJsRegex;
use serde_json::Value;
use std::collections::HashSet;

fn strip_first(xml: &str, pattern: &str) -> Result<String, String> {
    let re = TryJsRegex::new(pattern, "").map_err(|e| e.to_string())?;
    let Some(found) = re.try_exec(xml).map_err(|e| e.to_string())? else {
        return Ok(xml.to_owned());
    };
    let mut output = String::with_capacity(xml.len() - (found.end() - found.start()));
    output.push_str(&xml[..found.start()]);
    output.push_str(&xml[found.end()..]);
    Ok(output)
}
/// Gateway stage 1841–1869, after worksheet parts are written.
pub(super) fn remove_stale_chain(
    pkg: &mut PackageEditor<'_>,
    request: &Value,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    let changed_sheets = request
        .get("sheetPlan")
        .filter(|plan| !plan.is_null())
        .is_some_and(|plan| {
            plan.get("additions")
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty())
                || plan
                    .get("removals")
                    .and_then(Value::as_array)
                    .is_some_and(|v| !v.is_empty())
                || plan.get("orderChanged").and_then(Value::as_bool) == Some(true)
        });
    if !(changed_sheets
        || touched
            .iter()
            .any(|path| path.starts_with("xl/worksheets/")))
        || !pkg.has("xl/calcChain.xml")?
    {
        return Ok(());
    }
    pkg.remove("xl/calcChain.xml");
    let types = pkg.read_text("[Content_Types].xml")?;
    let stripped = strip_first(
        &types,
        r#"<Override\b[^>]*PartName="/xl/calcChain\.xml"[^>]*/>"#,
    )?;
    if stripped != *types {
        pkg.write("[Content_Types].xml", stripped)?;
        touched.insert("[Content_Types].xml".into());
    }
    let rels = pkg.read_text("xl/_rels/workbook.xml.rels")?;
    let stripped = strip_first(
        &rels,
        r#"<Relationship\b[^>]*Target="calcChain\.xml"[^>]*/>"#,
    )?;
    if stripped != *rels {
        pkg.write("xl/_rels/workbook.xml.rels", stripped)?;
        touched.insert("xl/_rels/workbook.xml.rels".into());
    }
    Ok(())
}
/// Force Excel to recalculate cached formulas when opening the saved workbook.
pub(super) fn ensure_full_calc(xml: &str) -> Result<String, String> {
    let already =
        TryJsRegex::new(r#"<calcPr\b[^>]*\bfullCalcOnLoad="1""#, "").map_err(|e| e.to_string())?;
    if already.try_test(xml).map_err(|e| e.to_string())? {
        return Ok(xml.to_owned());
    }
    let calc = TryJsRegex::new(r"<calcPr\b([^>]*?)(\/?>)", "").map_err(|e| e.to_string())?;
    if let Some(found) = calc.try_exec(xml).map_err(|e| e.to_string())? {
        let attributes = strip_first(found.get(1).unwrap_or(""), r#"\s*fullCalcOnLoad="[^"]*""#)?;
        return Ok(format!(
            "{}<calcPr{} fullCalcOnLoad=\"1\"{}{}",
            &xml[..found.start()],
            attributes,
            found.get(2).unwrap_or(""),
            &xml[found.end()..]
        ));
    }
    let incomplete = TryJsRegex::new(r"<calcPr\b", "").map_err(|e| e.to_string())?;
    if incomplete.try_test(xml).map_err(|e| e.to_string())? {
        return Ok(xml.to_owned());
    }
    let anchor = if xml.contains("</definedNames>") {
        "</definedNames>"
    } else {
        "</sheets>"
    };
    Ok(xml.replacen(
        anchor,
        &format!("{anchor}<calcPr fullCalcOnLoad=\"1\"/>"),
        1,
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removes_only_first_calculation_chain_declaration() {
        let input = r#"<Types><Override PartName="/xl/calcChain.xml"/><Override PartName="/other"/></Types>"#;
        assert_eq!(
            strip_first(
                input,
                r#"<Override\b[^>]*PartName="/xl/calcChain\.xml"[^>]*/>"#
            )
            .unwrap(),
            r#"<Types><Override PartName="/other"/></Types>"#
        );
    }
}
