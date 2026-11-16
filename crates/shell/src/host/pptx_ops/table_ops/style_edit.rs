use super::super::registry::{OpContext, field, op_name, record, string};
use super::{number, resolve};
use jscompat::JsNum;
use pptx_engine::table_edit::{self, CellRef, TableStyleEdit};
use pptx_engine::table_style::{BUILTIN_TABLE_STYLES, resolve_builtin_table_style_id};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;

fn present(v: Option<&IpcValue>) -> bool {
    !matches!(v, None | Some(IpcValue::Undefined | IpcValue::Null))
}
fn truthy(v: &IpcValue) -> bool {
    match v {
        IpcValue::Undefined | IpcValue::Null | IpcValue::Bool(false) => false,
        IpcValue::Number(n) => *n != 0.0 && !n.is_nan(),
        IpcValue::String(s) => !s.is_empty(),
        _ => true,
    }
}
fn hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}
fn style(
    op: &IpcValue,
) -> Result<(TableStyleEdit, IpcValue, Option<(&'static str, String)>), String> {
    let mut edit = TableStyleEdit::default();
    let mut after = Vec::new();
    let mut part = None;
    if present(field(op, "styleName")) {
        let name = field(op, "styleName").and_then(string).unwrap_or("");
        let preset = table_edit::table_style_preset(name).ok_or_else(|| {
            format!(
                "op \"setTableStyle\": unknown styleName \"{name}\". Presets: {}.",
                table_edit::TABLE_STYLE_PRESETS
                    .iter()
                    .map(|p| p.0)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        edit.tbl_pr_xml = Some(preset.tbl_pr_xml.clone());
        edit.clear_direct_formatting = Some(true);
        after.push((
            "tblPrXml".into(),
            IpcValue::String(preset.tbl_pr_xml.clone()),
        ));
        after.push(("clearDirectFormatting".into(), IpcValue::Bool(true)));
        if let Some(border) = &preset.border {
            edit.border_preset = Some(Some("all".into()));
            edit.border_color = Some(border.color.into());
            edit.border_width_emu = Some(border.width_emu);
            after.push(("borderPreset".into(), IpcValue::String("all".into())));
            after.push(("borderColor".into(), IpcValue::String(border.color.into())));
            after.push((
                "borderWidthEmu".into(),
                IpcValue::Number(border.width_emu.0),
            ));
        }
        if let (Some(id), Some(xml)) = (preset.style_id, preset.style_def_xml.as_ref()) {
            part = Some((id, xml.clone()));
        }
        return Ok((edit, IpcValue::Object(after), part));
    }
    let fields = [
        "styleId",
        "firstRow",
        "lastRow",
        "firstCol",
        "lastCol",
        "bandRow",
        "bandCol",
        "rtl",
        "shadingColor",
        "borderColor",
        "borderWidthPt",
        "borderPreset",
        "cells",
    ];
    if !fields.iter().any(|k| present(field(op, k))) {
        return Err("op \"setTableStyle\" needs \"styleName\", \"styleId\" (a built-in style name or GUID) or at least one of firstRow, lastRow, firstCol, lastCol, bandRow, bandCol, shadingColor, borderColor, borderWidthPt, borderPreset.".into());
    }
    if let Some(v) = field(op, "styleId").filter(|v| present(Some(v))) {
        let s = string(v).filter(|s| !s.trim().is_empty()).ok_or_else(|| {
            "op \"setTableStyle\": \"styleId\" must be a built-in style name or GUID.".to_owned()
        })?;
        let id = resolve_builtin_table_style_id(s).ok_or_else(|| {
            format!(
                "op \"setTableStyle\": unknown styleId \"{s}\". Built-in styles: {}.",
                BUILTIN_TABLE_STYLES
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        edit.style_id = Some(id.into());
        after.push(("styleId".into(), IpcValue::String(id.into())));
        if !matches!(field(op, "keepFormatting"), Some(IpcValue::Bool(true))) {
            edit.clear_direct_formatting = Some(true);
            after.push(("clearDirectFormatting".into(), IpcValue::Bool(true)));
        }
    }
    for (key, slot) in [
        ("firstRow", &mut edit.first_row),
        ("lastRow", &mut edit.last_row),
        ("firstCol", &mut edit.first_col),
        ("lastCol", &mut edit.last_col),
        ("bandCol", &mut edit.band_col),
        ("bandRow", &mut edit.band_row),
        ("rtl", &mut edit.rtl),
    ] {
        if let Some(value) = field(op, key).filter(|v| present(Some(v))) {
            let b = truthy(value);
            *slot = Some(b);
            after.push((key.into(), IpcValue::Bool(b)));
        }
    }
    for key in ["shadingColor", "borderColor"] {
        if let Some(value) = field(op, key).filter(|v| present(Some(v))) {
            let s = string(value).unwrap_or("").trim();
            if !(hex(s) || key == "shadingColor" && s == "none") {
                return Err(format!(
                    "op \"setTableStyle\": {key} must be #RRGGBB{}.",
                    if key == "shadingColor" {
                        " or \"none\""
                    } else {
                        ""
                    }
                ));
            }
            if key == "shadingColor" {
                edit.shading_color = Some(Some(s.into()));
            } else {
                edit.border_color = Some(s.into());
            }
            after.push((key.into(), IpcValue::String(s.into())));
        }
    }
    if let Some(v) = field(op, "borderWidthPt").filter(|v| present(Some(v))) {
        let n = match v {
            IpcValue::Number(n) => *n,
            IpcValue::String(s) => s.trim().parse().unwrap_or(f64::NAN),
            IpcValue::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            _ => f64::NAN,
        };
        if !n.is_finite() || n <= 0.0 || n > 1584.0 {
            return Err(
                "op \"setTableStyle\": borderWidthPt must be a finite number > 0 and <= 1584."
                    .into(),
            );
        }
        let n = jscompat::math_round(n * 12700.0);
        edit.border_width_emu = Some(JsNum(n));
        after.push(("borderWidthEmu".into(), IpcValue::Number(n)));
    }
    if let Some(v) = field(op, "borderPreset").filter(|v| present(Some(v))) {
        let s = string(v)
            .filter(|s| *s == "all" || *s == "none")
            .ok_or_else(|| {
                "op \"setTableStyle\": borderPreset must be \"all\" or \"none\".".to_owned()
            })?;
        edit.border_preset = Some(Some(s.into()));
        after.push(("borderPreset".into(), IpcValue::String(s.into())));
    }
    if let Some(v) = field(op, "cells").filter(|v| present(Some(v))) {
        let IpcValue::Array(cells) = v else {
            return Err("op \"setTableStyle\": cells must be an array of { \"row\": number, \"col\": number } pairs.".into());
        };
        if cells.len() > 4096 {
            return Err(format!(
                "op \"setTableStyle\": cells takes at most 4096 entries, got {}.",
                cells.len()
            ));
        }
        let mut refs = Vec::new();
        let mut values = Vec::new();
        for (i, cell) in cells.iter().enumerate() {
            if !matches!(cell, IpcValue::Object(_) | IpcValue::Array(_)) {
                return Err(format!(
                    "op \"setTableStyle\": cells[{i}] must be a {{ row, col }} object."
                ));
            }
            let row = number(cell, "row");
            let col = number(cell, "col");
            if !row.is_some_and(|n| n.is_finite() && n.fract() == 0.0)
                || !col.is_some_and(|n| n.is_finite() && n.fract() == 0.0)
            {
                return Err(format!(
                    "op \"setTableStyle\": cells[{i}] row and col must be integers."
                ));
            }
            let row = row.unwrap_or_default();
            let col = col.unwrap_or_default();
            if row < 0.0 || col < 0.0 {
                return Err(format!(
                    "op \"setTableStyle\": cells[{i}] row and col must be >= 0."
                ));
            }
            refs.push(CellRef { row, col });
            values.push(IpcValue::Object(vec![
                ("row".into(), IpcValue::Number(row)),
                ("col".into(), IpcValue::Number(col)),
            ]));
        }
        edit.cells = Some(refs);
        after.push(("cells".into(), IpcValue::Array(values)));
    }
    Ok((edit, IpcValue::Object(after), part))
}
pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    if op_name(op) != "setTableStyle" {
        return None;
    }
    Some((|| {
        resolve(ctx, op)?;
        style(op)?;
        Ok(())
    })())
}
pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    if op_name(op) != "setTableStyle" {
        return None;
    }
    Some((|| {
        let (target, id) = resolve(ctx, op)?;
        let (edit, after, part) = style(op)?;
        if let Some((id, xml)) = part {
            pptx_engine::deck::tables::ensure_table_style_part(ctx.opened, id, &xml);
        }
        if !pptx_engine::deck::tables::edit_table_style(ctx.slide_mut(&target), &id, &edit) {
            return Err(format!(
                "op \"setTableStyle\": table \"{id}\" rejected the style edit."
            ));
        }
        let mut result = record(op);
        result.after = after;
        Ok(result)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn op(fields: Vec<(&str, IpcValue)>) -> IpcValue {
        IpcValue::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        )
    }
    #[test]
    fn style_cell_validation_reports_bad_coordinate() {
        let input = op(vec![(
            "cells",
            IpcValue::Array(vec![op(vec![
                ("row", IpcValue::Number(1.5)),
                ("col", IpcValue::Number(0.0)),
            ])]),
        )]);
        assert!(
            style(&input)
                .unwrap_err()
                .contains("cells[0] row and col must be integers")
        );
    }
    #[test]
    fn preset_carries_direct_format_reset() {
        let input = op(vec![("styleName", IpcValue::String("lightGrid".into()))]);
        let (edit, _, _) = style(&input).unwrap();
        assert_eq!(edit.clear_direct_formatting, Some(true));
        assert_eq!(edit.border_preset, Some(Some("all".into())));
    }
}
