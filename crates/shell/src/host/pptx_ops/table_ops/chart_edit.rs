use pptx_engine::deck::charts::{self, ChartEditPatch};
use pptx_engine::types::ElementType;
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;
use super::super::registry::{field, op_name, record, resolve_element, OpContext, SlideTarget};
fn resolve(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<(SlideTarget, String), String> {
    let (target, index) = resolve_element(ctx, op, false)?; let el = &ctx.slide(&target).elements.as_ref().expect("resolved elements")[index];
    if el.element_type() != Some(ElementType::Chart) { return Err(format!("op \"setChart\" targets a {} element — it only works on chart elements.", match el.element_type() { Some(ElementType::Text) => "text", Some(ElementType::Shape) => "shape", Some(ElementType::Picture) => "picture", Some(ElementType::Group) => "group", Some(ElementType::Table) => "table", _ => "passthrough" })); }
    Ok((target, el.id().to_owned()))
}
pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    if op_name(op) != "setChart" { return None; }
    Some((|| { resolve(op, ctx)?; if !matches!(field(op, "patch"), Some(IpcValue::Object(_) | IpcValue::Array(_))) { return Err("op \"setChart\" needs \"patch\": a chart edit object.".into()); } Ok(()) })())
}
pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    if op_name(op) != "setChart" { return None; }
    Some((|| { let (target, id) = resolve(op, ctx)?; let SlideTarget::Slide(index) = target else { unreachable!() }; let value = field(op, "patch").ok_or_else(|| "op \"setChart\" needs \"patch\": a chart edit object.".to_owned())?; let patch: ChartEditPatch = super::super::insert_ops::ipc_de::decode(value).map_err(|e| format!("TypeError: argument 3: {e}"))?;
        charts::mark_chart_editable(&mut ctx.opened.deck.slides[index], &id);
        if !charts::edit_chart_element(ctx.opened, index, &id, &patch)? { return Err(format!("op \"setChart\": chart \"{id}\" rejected the edit.")); }
        let mut result = record(op); result.after = value.clone(); Ok(result)
    })())
}
