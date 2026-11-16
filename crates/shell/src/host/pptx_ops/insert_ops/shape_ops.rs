use super::super::registry::{OpContext, field, op_name, record, resolve_slide, string};
use super::{number, rect};
use jscompat::{JsNum, JsObject};
use pptx_engine::insert::{
    self, BodyAutoFit, BodyWrap, CellAnchor, NewElementBodyPr, NewElementOptions, NewStroke,
};
use pptx_engine::types::Insets;
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;

fn validate_inner(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<(), String> {
    resolve_slide(ctx, op, true)?;
    rect(op)?;
    if field(op, "kind").and_then(string).is_none_or(str::is_empty) {
        return Err(
            "op \"addElement\" needs \"kind\": \"textbox\" or a preset geometry name.".into(),
        );
    }
    if let Some(v) = field(op, "adjustments").filter(|v| !matches!(v, IpcValue::Undefined)) {
        let valid = match v {
            IpcValue::Object(fields) => fields
                .iter()
                .all(|(_, v)| number(Some(v)).is_some_and(f64::is_finite)),
            IpcValue::Array(fields) => fields
                .iter()
                .all(|v| number(Some(v)).is_some_and(f64::is_finite)),
            _ => false,
        };
        if !valid {
            return Err("op \"addElement\": \"adjustments\" must map avLst guide names to finite numbers, e.g. {adj: 25000} for roundRect corner radius.".into());
        }
    }
    if let Some(v) = field(op, "bodyPr")
        .and_then(|v| field(v, "autoFit"))
        .filter(|v| !matches!(v, IpcValue::Undefined))
    {
        if !matches!(string(v), Some("shrink" | "resize")) {
            return Err("op \"addElement\": \"bodyPr.autoFit\" must be \"shrink\" (fit text on overflow) or \"resize\" (grow the shape).".into());
        }
    }
    Ok(())
}
pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    if op_name(op) == "addElement" {
        Some(validate_inner(op, ctx))
    } else {
        None
    }
}
pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    if op_name(op) != "addElement" {
        return None;
    }
    Some((|| {
        validate_inner(op, ctx)?;
        let target = resolve_slide(ctx, op, true)?;
        let paragraphs = match field(op, "paragraphs") {
            Some(value @ IpcValue::Array(a)) if !a.is_empty() => {
                let parsed: Vec<pptx_engine::types::Paragraph> = super::ipc_de::decode(value)
                    .map_err(|e| format!("TypeError: argument 0: {e}"))?;
                Some(pptx_engine::generate::tag_hangul_paragraphs(&parsed).unwrap_or(parsed))
            }
            _ => None,
        };
        let adjustments = match field(op, "adjustments") {
            Some(IpcValue::Object(a)) => {
                let mut obj = JsObject::new();
                for (key, value) in a {
                    if let Some(n) = number(Some(value)) {
                        obj.set(key, JsNum(n));
                    }
                }
                Some(obj)
            }
            Some(IpcValue::Array(a)) => {
                let mut obj = JsObject::new();
                for (index, value) in a.iter().enumerate() {
                    if let Some(n) = number(Some(value)) {
                        obj.set(index.to_string(), JsNum(n));
                    }
                }
                Some(obj)
            }
            _ => None,
        };
        let body_pr = field(op, "bodyPr").map(|v| {
            let insets_emu = field(v, "insetsEmu").and_then(|i| {
                Some(Insets {
                    l: JsNum(number(field(i, "l"))?),
                    t: JsNum(number(field(i, "t"))?),
                    r: JsNum(number(field(i, "r"))?),
                    b: JsNum(number(field(i, "b"))?),
                })
            });
            NewElementBodyPr {
                wrap: match field(v, "wrap").and_then(string) {
                    Some("square") => Some(BodyWrap::Square),
                    Some("none") => Some(BodyWrap::None),
                    _ => None,
                },
                anchor: match field(v, "anchor").and_then(string) {
                    Some("t") => Some(CellAnchor::T),
                    Some("ctr") => Some(CellAnchor::Ctr),
                    Some("b") => Some(CellAnchor::B),
                    _ => None,
                },
                insets_emu,
                auto_fit: match field(v, "autoFit").and_then(string) {
                    Some("shrink") => Some(BodyAutoFit::Shrink),
                    Some("resize") => Some(BodyAutoFit::Resize),
                    _ => None,
                },
            }
        });
        let stroke = field(op, "stroke").and_then(|v| {
            Some(NewStroke {
                color: field(v, "color").and_then(string)?.into(),
                width_emu: JsNum(number(field(v, "widthEmu"))?),
            })
        });
        let options = NewElementOptions {
            kind: field(op, "kind").and_then(string).unwrap_or("").into(),
            offset: rect(op)?,
            paragraphs,
            fill_color: field(op, "fill").and_then(string).map(str::to_owned),
            stroke,
            body_pr,
            adjustments,
            flip_h: None,
            flip_v: None,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis() as f64;
        let el = insert::add_element(
            ctx.slide_mut(&target),
            &options,
            &jscompat::random_uuid(),
            now,
        )?;
        let mut result = record(op);
        result.created =
            Some(vec![el.id.as_ref().map_or(IpcValue::Undefined, |id| {
                IpcValue::String(id.clone())
            })]);
        Ok(result)
    })())
}
