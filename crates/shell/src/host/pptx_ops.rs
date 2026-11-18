use std::collections::HashMap;

use pptx_engine::deck::{materialize_slide, try_patch_slide_xml, OpenedPptx};
use pptx_engine::identity::{ensure_creation_id, slide_durable_id};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::{Isolation, TxnFailure, TxnRecord, TxnRequest, TxnResult};

mod registry;
mod units;
mod withhold;
mod core_ops;
mod edit_text;
mod slide_ops;
mod element_ops;
mod insert_ops;
mod text_ops;
mod table_ops;
mod animation_ops;
mod arrange_ops;
mod equation_ops;

use registry::{field, op_name, resolve_element, resolve_slide, string, OpContext};

type Validation = fn(&IpcValue, &mut OpContext<'_>) -> Option<Result<(), String>>;
type Apply = fn(&IpcValue, &mut OpContext<'_>) -> Option<Result<TxnRecord, String>>;

const VALIDATORS: &[Validation] = &[
    core_ops::validate, slide_ops::validate, element_ops::validate, insert_ops::validate,
    text_ops::validate, table_ops::validate, animation_ops::validate, arrange_ops::validate,
    equation_ops::validate,
];
const APPLIERS: &[Apply] = &[
    core_ops::apply, slide_ops::apply, element_ops::apply, insert_ops::apply,
    text_ops::apply, table_ops::apply, animation_ops::apply, arrange_ops::apply,
    equation_ops::apply,
];

fn xml_safe(op: &IpcValue) -> Result<(), String> {
    fn walk(value: &IpcValue, name: &str, path: &str) -> Result<(), String> {
        match value {
            IpcValue::String(s) if s.chars().any(|c| matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}')) =>
                Err(format!("op \"{name}\": \"{path}\" contains characters XML 1.0 forbids (control chars like \\u000B or a lone surrogate) — strip them first (PDF-extracted text often carries \\u000B; replace it with \\n).")),
            IpcValue::Array(items) => {
                for (index, item) in items.iter().enumerate() { walk(item, name, &format!("{path}[{index}]"))?; }
                Ok(())
            }
            IpcValue::Object(fields) => {
                for (key, item) in fields {
                    let child = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                    walk(item, name, &child)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    if let IpcValue::Object(fields) = op {
        for (key, value) in fields {
            if key != "op" { walk(value, op_name(op), key)?; }
        }
    }
    Ok(())
}

fn usage(name: &str) -> Option<String> {
    const SOURCES: &[&str] = &[
        include_str!("../../../../packages/pptx-ops/src/prompts/ops/text.md"),
        include_str!("../../../../packages/pptx-ops/src/prompts/ops/element.md"),
        include_str!("../../../../packages/pptx-ops/src/prompts/ops/insert.md"),
        include_str!("../../../../packages/pptx-ops/src/prompts/ops/table.md"),
        include_str!("../../../../packages/pptx-ops/src/prompts/ops/slide.md"),
        include_str!("../../../../packages/pptx-ops/src/prompts/ops/deck.md"),
    ];
    for source in SOURCES {
        let mut matching = false;
        for line in source.lines() {
            if let Some(heading) = line.strip_prefix("### ") {
                matching = heading.split_whitespace().next() == Some(name);
            } else if matching && line.starts_with('`') && line.ends_with('`') {
                return Some(format!("Usage: {name} {}", line.trim_matches('`')));
            }
        }
    }
    None
}

fn guided(op: &IpcValue, error: String) -> String {
    match usage(op_name(op)) { Some(usage) => format!("{error}\n{usage}"), None => error }
}

fn plan_line(index: usize, op: &IpcValue) -> String {
    let mut line = format!("[{index}] {}", op_name(op));
    if let Some(target) = field(op, "target") {
        let slide = match field(target, "slide") {
            Some(IpcValue::Number(n)) => n.to_string(),
            Some(IpcValue::String(s)) => s.clone(),
            _ => "undefined".into(),
        };
        line.push_str(&format!(" s{slide}"));
        if let Some(id) = field(target, "el").and_then(string).filter(|s| !s.is_empty()) { line.push('/'); line.push_str(id); }
    }
    line
}

fn flush(ctx: &mut OpContext<'_>) -> Result<(), String> {
    if ctx.touched_parts.is_empty() { return Ok(()); }
    for part in &ctx.touched_parts {
        let slide = ctx.parts.get_mut(part).ok_or_else(|| format!("missing touched master part: {part}"))?;
        ctx.opened.archive.entries_mut().set(part.clone(), try_patch_slide_xml(slide)?.into_bytes());
    }
    for index in 0..ctx.opened.deck.slides.len() { materialize_slide(ctx.opened, index)?; }
    Ok(())
}

fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<TxnRecord, String> {
    let stamp = if field(op, "target").and_then(|t| field(t, "part")).is_none()
        && field(op, "target").and_then(|t| field(t, "slide")).is_some() {
        resolve_slide(ctx, op, false).ok().map(|target| slide_durable_id(ctx.slide(&target)))
    } else { None };
    if ctx.withhold { withhold::guard(op, ctx)?; }
    let mut result = APPLIERS.iter().find_map(|apply| apply(op, ctx))
        .unwrap_or_else(|| Err(format!("unknown op \"{}\".", op_name(op))))?;
    if let Some(stamp) = stamp { result.slide_id = Some(stamp); }
    if let Ok((target, index)) = resolve_element(ctx, op, true) {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|error| format!("could not create element identity: {error}"))?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let uuid = format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}", bytes[0],bytes[1],bytes[2],bytes[3],bytes[4],bytes[5],bytes[6],bytes[7],bytes[8],bytes[9],bytes[10],bytes[11],bytes[12],bytes[13],bytes[14],bytes[15]);
        if let Some(element) = ctx.slide_mut(&target).elements.as_mut().and_then(|elements| elements.get_mut(index)) {
            ensure_creation_id(element, &uuid)?;
        }
    }
    Ok(result)
}

pub(crate) fn run_txn(
    opened: &mut OpenedPptx,
    master_parts: &mut HashMap<String, pptx_engine::types::Slide>,
    req: &TxnRequest,
) -> TxnResult {
    let parts = req.parts.iter().filter_map(|path| master_parts.remove(path).map(|slide| (path.clone(), slide))).collect();
    let mut ctx = OpContext { opened, parts, touched_parts: Vec::new(), withhold: req.withhold };
    let result = run_in_context(&mut ctx, req);
    if result.applied {
        for part in &ctx.touched_parts {
            if !req.parts.contains(part) { master_parts.remove(part); }
        }
    }
    master_parts.extend(ctx.parts.into_iter().filter(|(path, _)| req.parts.contains(path)));
    result
}

fn run_in_context(ctx: &mut OpContext<'_>, req: &TxnRequest) -> TxnResult {
    if req.ops.is_empty() {
        return TxnResult { failures: Some(vec![TxnFailure { index: 0, op: IpcValue::Object(vec![("op".into(), IpcValue::String("(none)".into()))]), error: "ops must be a non-empty array.".into() }]), ..TxnResult::default() };
    }
    let mut ops = req.ops.clone();
    let mut failures = Vec::new();
    let mut plan = Vec::new();
    for (index, op) in ops.iter_mut().enumerate() {
        let validation = (|| {
            if field(op, "op").and_then(string).is_none() { return Err("each op must be an object with an \"op\" name field.".into()); }
            xml_safe(op)?;
            units::normalize(op, None, None);
            units::normalize_media(op)?;
            VALIDATORS.iter().find_map(|validate| validate(op, ctx)).unwrap_or_else(|| Err(format!("unknown op \"{}\".", op_name(op))))?;
            if ctx.withhold { withhold::guard(op, ctx)?; }
            Ok(())
        })();
        match validation {
            Ok(()) => plan.push(IpcValue::String(plan_line(index, op))),
            Err(error) => failures.push(TxnFailure { index, op: op.clone(), error: guided(op, error) }),
        }
    }
    if req.dry_run { return TxnResult { dry_run: true, plan: Some(IpcValue::Array(plan)), failures: (!failures.is_empty()).then_some(failures), ..TxnResult::default() }; }
    if req.isolation == Isolation::Atomic && !failures.is_empty() {
        for failure in &mut failures { failure.error.push_str(" Nothing was applied (atomic) — fix this op and resend the whole transaction."); }
        return TxnResult { failures: Some(failures), ..TxnResult::default() };
    }
    ctx.touched_parts.clear();
    let snapshot = (req.isolation == Isolation::Atomic).then(|| (ctx.opened.deck.clone(), ctx.opened.archive.entries().clone(), ctx.opened.save_snapshot.clone()));
    let mut records = Vec::new();
    for (index, op) in ops.iter().enumerate() {
        if failures.iter().any(|f| f.index == index) { continue; }
        match apply(op, ctx) {
            Ok(record) => records.push(record),
            Err(error) => {
                if let Some((deck, entries, saved)) = snapshot {
                    ctx.opened.deck = deck;
                    *ctx.opened.archive.entries_mut() = entries;
                    ctx.opened.save_snapshot = saved;
                    return TxnResult { failures: Some(vec![TxnFailure { index, op: op.clone(), error: guided(op, format!("{error} Nothing was applied (atomic) — fix this op and resend the whole transaction.")) }]), ..TxnResult::default() };
                }
                failures.push(TxnFailure { index, op: op.clone(), error: guided(op, error) });
            }
        }
    }
    if !records.is_empty() {
        if let Err(error) = flush(ctx) {
            if let Some((deck, entries, saved)) = snapshot {
                ctx.opened.deck = deck;
                *ctx.opened.archive.entries_mut() = entries;
                ctx.opened.save_snapshot = saved;
                return TxnResult { failures: Some(vec![TxnFailure { index: ops.len() - 1, op: ops[ops.len() - 1].clone(), error }]), ..TxnResult::default() };
            }
            failures.push(TxnFailure { index: ops.len() - 1, op: ops[ops.len() - 1].clone(), error });
        }
    }
    failures.sort_by_key(|f| f.index);
    TxnResult { applied: !records.is_empty(), records: Some(records), failures: (!failures.is_empty()).then_some(failures), ..TxnResult::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deck() -> OpenedPptx {
        let bytes = pptx_engine::blank::create_blank_pptx(0.0).unwrap();
        pptx_engine::deck::open_pptx(&bytes).unwrap()
    }

    fn op(name: &str) -> IpcValue {
        IpcValue::Object(vec![
            ("op".into(), IpcValue::String(name.into())),
            ("target".into(), IpcValue::Object(vec![("slide".into(), IpcValue::Number(0.0))])),
        ])
    }

    #[test]
    fn atomic_apply_error_restores_deck_and_archive() {
        let mut opened = deck();
        let before_deck = opened.deck.clone();
        let before_entries = opened.archive.entries().clone();
        let result = run_txn(&mut opened, &mut HashMap::new(), &TxnRequest::atomic(vec![op("duplicateSlide"), op("deleteSlide"), op("deleteSlide")]));
        assert!(!result.applied);
        assert_eq!(result.failures.unwrap()[0].index, 2);
        assert_eq!(opened.deck, before_deck);
        assert_eq!(opened.archive.entries().iter().collect::<Vec<_>>(), before_entries.iter().collect::<Vec<_>>());
    }

    #[test]
    fn planning_and_dry_run_leave_deck_unchanged() {
        let mut opened = deck();
        let before = opened.deck.clone();
        let request = TxnRequest::atomic(vec![op("duplicateSlide"), op("notAnOperation")]);
        let dry = run_txn(&mut opened, &mut HashMap::new(), &request.dry());
        assert!(dry.dry_run);
        assert!(!dry.applied);
        assert_eq!(dry.failures.unwrap()[0].index, 1);
        assert_eq!(opened.deck, before);
        let result = run_txn(&mut opened, &mut HashMap::new(), &request);
        assert!(!result.applied);
        assert_eq!(opened.deck, before);
    }

    #[test]
    fn per_op_keeps_valid_edits_and_reports_invalid_ones() {
        let mut opened = deck();
        let result = run_txn(&mut opened, &mut HashMap::new(), &TxnRequest::per_op(vec![op("notAnOperation"), op("duplicateSlide")]));
        assert!(result.applied);
        assert_eq!(result.failures.unwrap()[0].index, 0);
        assert_eq!(opened.deck.slides.len(), 2);
    }
}

#[cfg(test)]
mod withhold_tests {
    use super::*;
    use pptx_engine::notes::{notes_path_for_slide, set_slide_notes};
    use pptx_engine::redaction_xml::redact_ext_xml;

    #[test]
    fn withheld_notes_cannot_be_overwritten() {
        let bytes = pptx_engine::blank::create_blank_pptx(0.0).unwrap();
        let mut opened = pptx_engine::deck::open_pptx(&bytes).unwrap();
        assert!(set_slide_notes(&mut opened, 0, "private").unwrap());
        let slide_path = opened.deck.slides[0].path.as_deref().unwrap();
        let path = notes_path_for_slide(&opened.archive, slide_path).unwrap().unwrap();
        let original = opened.archive.read_text(&path).unwrap().into_owned();
        let marked = original.replace("</p:notes>", &format!("<a:extLst>{}</a:extLst></p:notes>", redact_ext_xml("private")));
        opened.archive.entries_mut().set(path.clone(), marked.clone().into_bytes());
        let op = IpcValue::Object(vec![
            ("op".into(), IpcValue::String("setNotes".into())),
            ("target".into(), IpcValue::Object(vec![("slide".into(), IpcValue::Number(0.0))])),
            ("text".into(), IpcValue::String("replacement".into())),
        ]);
        let mut request = TxnRequest::atomic(vec![op]);
        request.withhold = true;
        let result = run_txn(&mut opened, &mut HashMap::new(), &request);
        assert!(!result.applied);
        assert!(result.failures.unwrap()[0].error.contains("withheld from AI"));
        assert_eq!(opened.archive.read_text(&path).unwrap(), marked);
    }
}
