//! Workbook mutation planning over the streaming sidecar's package entries.
mod calculation;
mod cells;
mod charts;
mod conditional_format;
mod defined_names;
mod drawings;
mod external_links;
mod filters;
mod formulas;
mod guards;
mod hyperlinks;
mod normalize;
mod notes;
mod package;
mod page_setup;
mod pivots;
mod protection;
mod redaction;
mod sheet_allocate;
mod sheets;
mod slicers;
mod sparkline;
mod state;
mod structure;
mod style_entries;
mod styles;
mod tab_color;
mod tables;
mod theme;
mod validation;
mod xml;

pub(super) use normalize::normalize_ooxml_part_prefix;
pub(super) use package::{EntrySource, MutationPlan};
pub(super) use state::resolve_rel_target;

pub(super) fn validate_pivot_formula(formula: &str, fields: &[String]) -> Result<String, String> {
    pivots::formula::normalize(formula, fields)
}

/// Allocate a stylesheet only when one of the mutation phases needs it.
fn stylesheet_for_save(
    pkg: &mut PackageEditor<'_>,
    request: &serde_json::Value,
    touched: &mut std::collections::HashSet<String>,
) -> Result<Option<styles::StylesheetEditor>, String> {
    let items = |key: &str| request.get(key).and_then(serde_json::Value::as_array);
    let styled =
        items("edits").is_some_and(|edits| edits.iter().any(|edit| edit.get("style").is_some()));
    let conditional = items("cfStates").is_some_and(|states| !states.is_empty());
    let columns = items("structuralOps").is_some_and(|sheets| {
        sheets.iter().any(|sheet| {
            sheet
                .get("ops")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|ops| {
                    ops.iter().any(|op| {
                        op.get("kind").and_then(serde_json::Value::as_str) == Some("set-col-style")
                    })
                })
        })
    });
    if !(styled || conditional || columns) {
        return Ok(None);
    }
    if !pkg.has("xl/styles.xml")? {
        styles::add_default_stylesheet(pkg, touched)?;
    }
    let xml = pkg.read_text("xl/styles.xml")?;
    let short_date = pkg.short_date_pattern()?;
    Ok(Some(styles::StylesheetEditor::new(&xml, &short_date)?))
}
use package::PackageEditor;

/// Apply final calculation and redaction metadata to the final sheet order.
fn finish_workbook(
    pkg: &mut PackageEditor<'_>,
    state: &mut state::PlannerState,
    redactions: Option<&[serde_json::Value]>,
    touched: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    let mut workbook = calculation::ensure_full_calc(&state.workbook_xml)?;
    if let Some(redactions) = redactions {
        let names: Vec<_> = xml::root_elements(&workbook, "sheet")?
            .into_iter()
            .filter_map(|sheet| sheet.attribute("name").map(str::to_owned))
            .collect();
        workbook = redaction::apply_mirror(&workbook, redactions, &names)?;
    }
    if workbook != *state.workbook_original {
        pkg.write("xl/workbook.xml", workbook)?;
        touched.insert("xl/workbook.xml".into());
    }
    Ok(())
}

/// Apply final workbook-level name, structure-protection and theme changes.
fn apply_workbook_properties(
    pkg: &mut PackageEditor<'_>,
    state: &mut state::PlannerState,
    request: &serde_json::Value,
    touched: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    if let Some(names) = request
        .get("definedNamesState")
        .filter(|value| !value.is_null())
    {
        state.workbook_xml = std::sync::Arc::new(defined_names::apply(&state.workbook_xml, names)?);
    }
    if let Some(protection) = request
        .get("workbookProtectionState")
        .filter(|value| !value.is_null())
    {
        let locked = protection
            .get("lockStructure")
            .and_then(serde_json::Value::as_bool)
            .ok_or("Workbook protection state has no lockStructure.")?;
        state.workbook_xml =
            std::sync::Arc::new(protection::workbook(&state.workbook_xml, locked)?);
    }
    theme::write(pkg, request, touched)
}

fn dynamic_array_metadata(
    pkg: &mut PackageEditor<'_>,
    request: &serde_json::Value,
    touched: &mut std::collections::HashSet<String>,
) -> Result<Option<usize>, String> {
    let spills = request
        .get("edits")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|edits| {
            edits.iter().any(|edit| {
                edit.get("writeValue").and_then(serde_json::Value::as_bool) == Some(true)
                    && edit
                        .get("cell")
                        .and_then(|cell| cell.get("formula"))
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(formulas::spills_dynamic_array)
            })
        });
    if spills {
        formulas::ensure_dynamic_array_metadata(pkg, touched)
    } else {
        Ok(None)
    }
}

/// Plan the same ordered workbook mutations used by the XLSX gateway.
pub(super) fn plan(
    source: &mut dyn EntrySource,
    request: &serde_json::Value,
) -> Result<MutationPlan, String> {
    guards::validate(request)?;
    let mut pkg = PackageEditor::new(source);
    let mut touched = std::collections::HashSet::new();
    let mut state = state::PlannerState::new(&mut pkg)?;
    sheet_allocate::allocate(&mut pkg, &mut state, request, &mut touched)?;
    state.load_referenced_sheets(&mut pkg, request)?;
    pivots::apply_refresh_phase(&mut pkg, &mut state, request, &mut touched)?;

    let mut stylesheet = stylesheet_for_save(&mut pkg, request, &mut touched)?;
    let pending = structure::apply(
        &mut pkg,
        &mut state,
        request,
        stylesheet.as_mut(),
        &mut touched,
    )?;
    let dynamic_cm = dynamic_array_metadata(&mut pkg, request, &mut touched)?;
    let redactions = redaction::prepare(&mut pkg, &mut touched, request, &state.workbook_original)?;
    cells::apply_cell_edits(&mut state, request, stylesheet.as_mut(), dynamic_cm)?;
    structure::sync_tables(&mut pkg, &mut state, pending, &mut touched)?;
    hyperlinks::apply(&mut pkg, &mut state, request, &mut touched)?;
    conditional_format::apply(&mut state, request, stylesheet.as_mut())?;
    validation::apply(&mut state, request)?;
    protection::sheets(&mut state, request)?;
    tab_color::apply(&mut state, request)?;
    protection::ranges(&mut state, request)?;
    page_setup::worksheets(&mut state, request)?;
    state.flush_worksheets(&mut pkg, &mut touched)?;
    if let Some(stylesheet) = stylesheet.filter(|stylesheet| stylesheet.changed()) {
        pkg.write("xl/styles.xml", stylesheet.serialize())?;
        touched.insert("xl/styles.xml".into());
    }

    charts::apply(&mut pkg, &mut state, request, &mut touched)?;
    drawings::apply(&mut pkg, &mut state, request, &mut touched)?;
    notes::apply(&mut pkg, &mut state, request, &mut touched)?;
    let refresh_paths: Vec<String> = request
        .get("pivotCacheRefreshPaths")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .map(|path| {
            path.as_str()
                .map(str::to_owned)
                .ok_or("Invalid pivot cache path.")
        })
        .collect::<Result<_, _>>()?;
    pivots::refresh_caches(&mut pkg, &refresh_paths, &mut touched)?;
    tables::apply(&mut pkg, &mut state, request, &mut touched)?;
    filters::apply(&mut pkg, &mut state, request, &mut touched)?;
    sparkline::apply(&mut pkg, &mut state, request, &mut touched)?;
    calculation::remove_stale_chain(&mut pkg, request, &mut touched)?;
    if let Some(sheet_plan) = request.get("sheetPlan").filter(|value| !value.is_null()) {
        sheets::apply_sheet_plan(&mut pkg, &mut state, sheet_plan, &mut touched)?;
    }
    apply_workbook_properties(&mut pkg, &mut state, request, &mut touched)?;
    page_setup::print_names(&mut state, request)?;
    pivots::apply_additions_phase(&mut pkg, &mut state, request, &mut touched)?;
    slicers::apply_slicer_phase(&mut pkg, &mut state, request, &mut touched)?;
    finish_workbook(&mut pkg, &mut state, redactions.as_deref(), &mut touched)?;
    external_links::apply(&mut pkg, &mut state, request, &mut touched)?;
    Ok(pkg.to_plan(touched))
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::sync::Arc;

    struct Source;
    impl EntrySource for Source {
        fn paths(&mut self) -> Result<Vec<String>, String> {
            Ok(vec!["xl/workbook.xml".into()])
        }
        fn read_text(&mut self, path: &str) -> Result<Arc<String>, String> {
            if path == "xl/workbook.xml" {
                Ok(Arc::new("<workbook><sheets></sheets></workbook>".into()))
            } else {
                Err(format!("Workbook is missing {path}."))
            }
        }
        fn can_patch(&mut self, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn contains_text(&mut self, _: &str, _: &str) -> Result<bool, String> {
            Ok(false)
        }
        fn short_date_pattern(&mut self) -> Result<String, String> {
            Ok("m/d/yyyy".into())
        }
        fn release_text(&mut self, _: &str) {}
    }

    #[test]
    fn empty_mutations_force_recalculation_without_fabricating_parts() {
        let plan = plan(&mut Source, &serde_json::json!({})).unwrap();
        assert_eq!(plan.touched_entries, vec!["xl/workbook.xml"]);
        assert!(plan.added.is_empty() && plan.removed_entries.is_empty());
        assert_eq!(
            plan.replaced,
            vec![(
                "xl/workbook.xml".into(),
                "<workbook><sheets></sheets><calcPr fullCalcOnLoad=\"1\"/></workbook>".into()
            )]
        );
    }
}
