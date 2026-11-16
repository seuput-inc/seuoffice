use super::super::registry::{field, string};
use pptx_engine::deck::{ElementFontPatch, ParagraphFormatPatch};
use pptx_engine::types::TextAlign;
use shell_contract::IpcValue;

fn number(v: Option<&IpcValue>) -> Option<f64> {
    match v {
        Some(IpcValue::Number(n)) => Some(*n),
        _ => None,
    }
}
fn bool_value(v: Option<&IpcValue>) -> Option<bool> {
    match v {
        Some(IpcValue::Bool(b)) => Some(*b),
        _ => None,
    }
}
fn present(v: Option<&IpcValue>) -> bool {
    !matches!(v, None | Some(IpcValue::Undefined))
}
fn nonnull(v: Option<&IpcValue>) -> bool {
    !matches!(v, None | Some(IpcValue::Undefined | IpcValue::Null))
}
fn finite(value: Option<&IpcValue>, op: &str, field: &str) -> Result<f64, String> {
    number(value)
        .filter(|n| n.is_finite())
        .ok_or_else(|| format!("op \"{op}\": \"{field}\" must be a finite number."))
}
fn hex(value: Option<&IpcValue>, op: &str, name: &str) -> Result<(), String> {
    let valid = value.and_then(string).is_some_and(|s| {
        let s = s.trim();
        let s = s.strip_prefix('#').unwrap_or(s);
        (s.len() == 6 || s.len() == 8) && s.bytes().all(|c| c.is_ascii_hexdigit())
    });
    let got = match value {
        Some(IpcValue::String(s)) => serde_json::to_string(s).unwrap_or_default(),
        Some(IpcValue::Number(n)) if n.is_finite() => jscompat::number_to_string(*n),
        Some(IpcValue::Bool(b)) => b.to_string(),
        Some(IpcValue::Null | IpcValue::Number(_)) => "null".into(),
        None | Some(IpcValue::Undefined) => "undefined".into(),
        Some(IpcValue::Array(_)) => "[]".into(),
        _ => "{}".into(),
    };
    if valid {
        Ok(())
    } else {
        Err(format!(
            "op \"{op}\": \"{name}\" must be a hex color \"#RRGGBB\" (or \"#RRGGBBAA\"), got {got}."
        ))
    }
}
pub(super) fn validate_font(op: &IpcValue) -> Result<(), String> {
    let Some(font @ (IpcValue::Object(_) | IpcValue::Array(_))) = field(op, "font") else {
        return Err("op \"setFont\" needs \"font\": an ElementFontPatch object.".into());
    };
    if present(field(font, "color")) {
        hex(field(font, "color"), "setFont", "font.color")?;
    }
    if nonnull(field(font, "highlight")) {
        hex(field(font, "highlight"), "setFont", "font.highlight")?;
    }
    if nonnull(field(font, "letterSpacingPt")) {
        let n = finite(
            field(font, "letterSpacingPt"),
            "setFont",
            "font.letterSpacingPt",
        )?;
        if n.abs() > 4000.0 {
            return Err(
                "op \"setFont\": \"font.letterSpacingPt\" must be -4000..4000 (points).".into(),
            );
        }
    }
    if present(field(font, "fontSizePt")) {
        let n = finite(field(font, "fontSizePt"), "setFont", "font.fontSizePt")?;
        if !(1.0..=4000.0).contains(&n) {
            return Err("op \"setFont\": \"font.fontSizePt\" must be 1..4000 (points).".into());
        }
    }
    if present(field(font, "fontSizeStep")) {
        let step = field(font, "fontSizeStep");
        if present(field(font, "fontSizePt"))
            || !matches!(step.and_then(|s| number(field(s, "dir"))), Some(1.0 | -1.0))
            || !matches!(
                step.and_then(|s| field(s, "mode")).and_then(string),
                Some("point" | "ladder")
            )
        {
            return Err("op \"setFont\": \"font.fontSizeStep\" must be { dir: 1 | -1, mode: \"ladder\" | \"point\" } and cannot be combined with \"font.fontSizePt\".".into());
        }
    }
    Ok(())
}
pub(super) fn font_patch(font: &IpcValue) -> ElementFontPatch {
    let nullable_string = |v: Option<&IpcValue>| {
        if present(v) {
            Some(v.and_then(string).map(str::to_owned))
        } else {
            None
        }
    };
    let nullable_number = |v: Option<&IpcValue>| if present(v) { Some(number(v)) } else { None };
    ElementFontPatch {
        font_family: field(font, "fontFamily")
            .and_then(string)
            .map(str::to_owned),
        font_size_pt: number(field(font, "fontSizePt")),
        strike: bool_value(field(font, "strike")),
        bold: bool_value(field(font, "bold")),
        italic: bool_value(field(font, "italic")),
        underline: bool_value(field(font, "underline")),
        color: field(font, "color").and_then(string).map(str::to_owned),
        highlight: nullable_string(field(font, "highlight")),
        letter_spacing_pt: nullable_number(field(font, "letterSpacingPt")),
    }
}
pub(super) fn font_step(step: &IpcValue, value: Option<f64>) -> f64 {
    const LADDER: &[f64] = &[
        8., 9., 10., 10.5, 11., 12., 14., 16., 18., 20., 24., 28., 32., 36., 40., 44., 48., 54.,
        60., 66., 72., 80., 88., 96.,
    ];
    let from = value.filter(|n| n.is_finite()).unwrap_or(18.0);
    let dir = number(field(step, "dir")).unwrap_or(1.0);
    if field(step, "mode").and_then(string) == Some("point") {
        return (from + dir).clamp(1.0, 4000.0);
    }
    if dir > 0.0 {
        if from >= 96.0 {
            (from + 8.0).min(4000.0)
        } else if from < 8.0 {
            (from + 1.0).min(8.0)
        } else {
            LADDER.iter().copied().find(|n| *n > from).unwrap_or(18.0)
        }
    } else if from > 96.0 {
        (from - 8.0).max(96.0)
    } else if from <= 8.0 {
        (from - 1.0).max(1.0)
    } else {
        LADDER
            .iter()
            .rev()
            .copied()
            .find(|n| *n < from)
            .unwrap_or(8.0)
    }
}
pub(super) fn validate_format(op: &IpcValue) -> Result<(), String> {
    let Some(fmt @ (IpcValue::Object(_) | IpcValue::Array(_))) = field(op, "format") else {
        return Err(
            "op \"setParagraphFormat\" needs \"format\": a ParagraphFormatPatch object.".into(),
        );
    };
    for (key, min, max) in [
        ("lineSpacingPct", 0., 13200.),
        ("spaceBeforePt", 0., 1584.),
        ("spaceAfterPt", 0., 1584.),
        ("bulletSizePct", 25., 400.),
        ("bulletHangEmu", 0., 51206400.),
    ] {
        if present(field(fmt, key)) {
            let label = format!("format.{key}");
            let n = finite(field(fmt, key), "setParagraphFormat", &label)?;
            if n < min || n > max {
                return Err(format!(
                    "op \"setParagraphFormat\": \"{label}\" must be {min}..{max}."
                ));
            }
        }
    }
    if present(field(fmt, "bulletColor")) {
        hex(
            field(fmt, "bulletColor"),
            "setParagraphFormat",
            "format.bulletColor",
        )?;
    }
    for key in ["bulletChar", "bulletFont", "numType"] {
        if present(field(fmt, key)) && field(fmt, key).and_then(string).is_none() {
            return Err(format!(
                "op \"setParagraphFormat\": \"format.{key}\" must be a string."
            ));
        }
    }
    if present(field(fmt, "startAt")) {
        let n = finite(
            field(fmt, "startAt"),
            "setParagraphFormat",
            "format.startAt",
        )?;
        if n < 1.0 || n.fract() != 0.0 {
            return Err(
                "op \"setParagraphFormat\": \"format.startAt\" must be an integer >= 1.".into(),
            );
        }
    }
    if field(fmt, "bullet").and_then(string) == Some("blip") {
        let image = field(fmt, "bulletImage");
        if image
            .and_then(|i| field(i, "base64"))
            .and_then(string)
            .is_none()
            || image
                .and_then(|i| field(i, "ext"))
                .and_then(string)
                .is_none()
        {
            return Err("op \"setParagraphFormat\": bullet \"blip\" needs \"format.bulletImage\": { base64, ext }.".into());
        }
    }
    if present(field(fmt, "rtl")) && bool_value(field(fmt, "rtl")).is_none() {
        return Err("op \"setParagraphFormat\": \"format.rtl\" must be a boolean.".into());
    }
    Ok(())
}
pub(super) fn paragraph_patch(fmt: &IpcValue) -> ParagraphFormatPatch {
    let s = |key| field(fmt, key).and_then(string).map(str::to_owned);
    ParagraphFormatPatch {
        bullet: s("bullet"),
        num_type: s("numType"),
        start_at: number(field(fmt, "startAt")),
        bullet_blip: None,
        bullet_char: s("bulletChar"),
        bullet_font: s("bulletFont"),
        bullet_hang_emu: number(field(fmt, "bulletHangEmu")),
        bullet_size_pct: number(field(fmt, "bulletSizePct")),
        bullet_color: s("bulletColor"),
        line_spacing_pct: number(field(fmt, "lineSpacingPct")),
        space_before_pt: number(field(fmt, "spaceBeforePt")),
        space_after_pt: number(field(fmt, "spaceAfterPt")),
        align: match field(fmt, "align").and_then(string) {
            Some("left") => Some(TextAlign::Left),
            Some("center") => Some(TextAlign::Center),
            Some("right") => Some(TextAlign::Right),
            Some("justify") => Some(TextAlign::Justify),
            _ => None,
        },
        rtl: bool_value(field(fmt, "rtl")),
        indent_delta: number(field(fmt, "indentDelta")),
    }
}
