use super::super::registry::{
    OpContext, SlideTarget, field, op_name, record, resolve_slide, string,
};
use super::{number, rect};
use pptx_engine::chart_insert::{self, NewChartOptions};
use pptx_engine::smartart::{self, NewSmartArtOptions};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;

fn array<'a>(op: &'a IpcValue, key: &str) -> Option<&'a [IpcValue]> {
    match field(op, key) {
        Some(IpcValue::Array(items)) => Some(items),
        _ => None,
    }
}
fn known(name: &str) -> bool {
    matches!(name, "addChart" | "addSmartArt")
}
fn validate_inner(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<(), String> {
    resolve_slide(ctx, op, false)?;
    rect(op)?;
    if op_name(op) == "addSmartArt" {
        if field(op, "layout").and_then(string).is_none() || array(op, "items").is_none() {
            return Err("op \"addSmartArt\" needs \"layout\" and \"items\".".into());
        }
        return Ok(());
    }
    if field(op, "kind").and_then(string).is_none() {
        return Err("op \"addChart\" needs \"kind\".".into());
    }
    if array(op, "categories").is_none_or(|a| a.is_empty()) {
        return Err("op \"addChart\" needs \"categories\": at least one category label.".into());
    }
    if array(op, "series")
        .is_none_or(|a| a.is_empty() || a.iter().any(|s| array(s, "values").is_none()))
    {
        return Err("op \"addChart\" needs \"series\": at least one {name, values[]}.".into());
    }
    if let Some(value) = field(op, "colorScheme").filter(|v| !matches!(v, IpcValue::Undefined)) {
        let IpcValue::Array(colors) = value else {
            return Err("op \"addChart\": \"colorScheme\" must be a non-empty array of \"#RRGGBB\" series colors (cycled over the series).".into());
        };
        if colors.is_empty() {
            return Err("op \"addChart\": \"colorScheme\" must be a non-empty array of \"#RRGGBB\" series colors (cycled over the series).".into());
        }
        for color in colors {
            if !string(color).is_some_and(|s| {
                let s = s.trim().strip_prefix('#').unwrap_or(s.trim());
                matches!(s.len(), 6 | 8) && s.bytes().all(|b| b.is_ascii_hexdigit())
            }) {
                let got = match color {
                    IpcValue::String(s) => serde_json::to_string(s).unwrap_or_default(),
                    IpcValue::Null => "null".into(),
                    IpcValue::Bool(b) => b.to_string(),
                    IpcValue::Number(n) if n.is_finite() => jscompat::number_to_string(*n),
                    IpcValue::Number(_) => "null".into(),
                    IpcValue::Undefined => "undefined".into(),
                    _ => "[object Object]".into(),
                };
                return Err(format!(
                    "op \"addChart\": \"colorScheme[]\" must be a hex color \"#RRGGBB\" (or \"#RRGGBBAA\"), got {got}."
                ));
            }
        }
    }
    if field(op, "holeSizePct").is_some_and(|v| !matches!(v, IpcValue::Undefined))
        && !number(field(op, "holeSizePct")).is_some_and(f64::is_finite)
    {
        return Err("op \"addChart\": \"holeSizePct\" must be a finite number.".into());
    }
    Ok(())
}
pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    if known(op_name(op)) {
        Some(validate_inner(op, ctx))
    } else {
        None
    }
}
pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    if !known(op_name(op)) {
        return None;
    }
    Some((|| {
        validate_inner(op, ctx)?;
        let SlideTarget::Slide(index) = resolve_slide(ctx, op, false)? else {
            unreachable!()
        };
        let uuid = jscompat::random_uuid();
        let mut result = record(op);
        let id = if op_name(op) == "addSmartArt" {
            let options = NewSmartArtOptions {
                layout: field(op, "layout").and_then(string).unwrap_or("").into(),
                items: super::ipc_de::decode(field(op, "items").expect("validated items"))
                    .map_err(|e| format!("TypeError: argument 2: {e}"))?,
                offset: rect(op)?,
            };
            smartart::add_smart_art(ctx.opened, index, &options, &uuid)?
                .ok_or_else(|| "op \"addSmartArt\": the diagram could not be inserted.".to_owned())?
                .element_id
                .map_or(IpcValue::Undefined, IpcValue::String)
        } else {
            let offset = rect(op)?;
            let mut fields = vec![(
                "kind".into(),
                field(op, "kind").expect("validated kind").clone(),
            )];
            if let Some(v) = field(op, "barDir").filter(|v| match v {
                IpcValue::Undefined | IpcValue::Null | IpcValue::Bool(false) => false,
                IpcValue::String(s) => !s.is_empty(),
                IpcValue::Number(n) => *n != 0.0 && !n.is_nan(),
                _ => true,
            }) {
                fields.push(("barDir".into(), v.clone()));
            }
            if let Some(v) = field(op, "title")
                .and_then(string)
                .filter(|s| !s.is_empty())
            {
                fields.push(("title".into(), IpcValue::String(v.into())));
            }
            fields.push((
                "categories".into(),
                field(op, "categories")
                    .expect("validated categories")
                    .clone(),
            ));
            fields.push((
                "series".into(),
                field(op, "series").expect("validated series").clone(),
            ));
            fields.push((
                "offset".into(),
                IpcValue::Object(vec![
                    ("x".into(), IpcValue::Number(offset.x.0)),
                    ("y".into(), IpcValue::Number(offset.y.0)),
                    ("cx".into(), IpcValue::Number(offset.cx.0)),
                    ("cy".into(), IpcValue::Number(offset.cy.0)),
                ]),
            ));
            if let Some(v) = field(op, "colorScheme").filter(|v| {
                !matches!(
                    v,
                    IpcValue::Undefined | IpcValue::Null | IpcValue::Bool(false)
                )
            }) {
                fields.push(("colorScheme".into(), v.clone()));
            }
            if let Some(n) = number(field(op, "holeSizePct")) {
                fields.push(("holeSizePct".into(), IpcValue::Number(n)));
            }
            let options: NewChartOptions = super::ipc_de::decode(&IpcValue::Object(fields))
                .map_err(|e| format!("TypeError: argument 2: {e}"))?;
            IpcValue::String(
                chart_insert::add_chart(ctx.opened, index, &options, &uuid)?.ok_or_else(|| {
                    "op \"addChart\": the chart could not be inserted.".to_owned()
                })?,
            )
        };
        result.created = Some(vec![id]);
        Ok(result)
    })())
}
