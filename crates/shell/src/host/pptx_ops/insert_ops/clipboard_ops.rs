use pptx_engine::deck::clipboard::{self, ElementClipboardItem, ShiftEmu};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;
use super::number;
use super::super::registry::{field, op_name, record, resolve_slide, OpContext, SlideTarget};

fn items(op: &IpcValue) -> Option<&[IpcValue]> { match field(op, "items") { Some(IpcValue::Array(a)) => Some(a), _ => None } }
fn validate_inner(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<(), String> {
    resolve_slide(ctx, op, false)?;
    if items(op).is_none_or(|items| items.is_empty()) { return Err("op \"pasteElements\" needs \"items\": copied element data.".into()); }
    if number(field(op, "dx")).is_none() || number(field(op, "dy")).is_none() { return Err("op \"pasteElements\" needs \"dx\"/\"dy\" (EMU offsets).".into()); }
    Ok(())
}
pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> { if op_name(op) == "pasteElements" { Some(validate_inner(op, ctx)) } else { None } }
pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    if op_name(op) != "pasteElements" { return None; }
    Some((|| { validate_inner(op, ctx)?; let SlideTarget::Slide(index) = resolve_slide(ctx, op, false)? else { unreachable!() };
        let items: Vec<ElementClipboardItem> = super::ipc_de::decode(field(op, "items").expect("validated items")).map_err(|e| format!("TypeError: argument 2: {e}"))?;
        let ids = clipboard::paste_elements(ctx.opened, index, &items, ShiftEmu { dx: number(field(op, "dx")).unwrap_or_default(), dy: number(field(op, "dy")).unwrap_or_default() })?.ok_or_else(|| "op \"pasteElements\": the clipboard items could not be pasted.".to_owned())?;
        let mut result = record(op); result.created = Some(ids.into_iter().map(IpcValue::String).collect()); Ok(result)
    })())
}
