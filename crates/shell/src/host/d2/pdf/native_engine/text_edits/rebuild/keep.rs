//! Preserve untouched source objects, including their own fonts and spacing.
use super::super::{
    PdfiumSession, TextObject, buffer, call, field, integer, offset, ordered, reference, text,
};
use super::fold::{fold, index_of, whitespace_clamp};
use super::style::RunStyle;
use serde_json::json;
use shell_contract::IpcValue;
use std::collections::HashMap;

pub(super) struct Keep<'a> {
    pub src: &'a TextObject,
    pub start: usize,
    pub end: usize,
    pub advance: f64,
    pub x: f64,
    pub y: f64,
    pub color: Option<[u8; 3]>,
}

pub(super) fn plan<'a>(
    session: &mut PdfiumSession,
    text_page: u64,
    matches: &'a [TextObject],
    replacement: &str,
    styles: Option<&[Option<RunStyle>]>,
    color: Option<[u8; 3]>,
    edit: &IpcValue,
) -> Result<Option<Vec<Keep<'a>>>, String> {
    let mut chars: HashMap<u64, Vec<i64>> =
        matches.iter().map(|o| (o.handle, Vec::new())).collect();
    let total = integer(call(
        session,
        "FPDFText_CountChars",
        &[reference(text_page)],
    )?)?;
    for i in 0..total {
        let value = call(
            session,
            "FPDFText_GetTextObject",
            &[reference(text_page), json!(i)],
        )?;
        if let Some(id) = value.get("ref").and_then(|v| v.as_u64()) {
            if let Some(list) = chars.get_mut(&id) {
                list.push(i);
            }
        }
    }
    let mut units = Vec::new();
    let mut counts = Vec::new();
    let mut geometry = Vec::new();
    let sorted = ordered(matches);
    for obj in &sorted {
        let indices = chars.get(&obj.handle).map(Vec::as_slice).unwrap_or(&[]);
        let count = obj.text.chars().count();
        let trimmed = obj
            .text
            .trim_end_matches(jscompat::is_js_space)
            .chars()
            .count();
        let geo = if !indices.is_empty() && (indices.len() == count || indices.len() == trimmed) {
            buffer(session, 32, |s, b| {
                let mut advance = 0.0;
                for &i in indices {
                    if integer(call(
                        s,
                        "FPDFText_GetLooseCharBox",
                        &[reference(text_page), json!(i), reference(b)],
                    )?)? == 0
                    {
                        return Ok(None);
                    }
                    let bytes = s.read(b, 0, 16).map_err(|e| e.to_string())?;
                    let left = f32::from_le_bytes(
                        bytes[0..4]
                            .try_into()
                            .map_err(|_| "invalid character box")?,
                    );
                    let right = f32::from_le_bytes(
                        bytes[8..12]
                            .try_into()
                            .map_err(|_| "invalid character box")?,
                    );
                    advance += f64::from(right - left);
                }
                if !advance.is_finite()
                    || integer(call(
                        s,
                        "FPDFText_GetCharOrigin",
                        &[
                            reference(text_page),
                            json!(indices[0]),
                            offset(b, 16),
                            offset(b, 24),
                        ],
                    )?)? == 0
                {
                    return Ok(None);
                }
                let bytes = s.read(b, 16, 16).map_err(|e| e.to_string())?;
                let x = f64::from_le_bytes(
                    bytes[0..8]
                        .try_into()
                        .map_err(|_| "invalid character origin")?,
                );
                let y = f64::from_le_bytes(
                    bytes[8..16]
                        .try_into()
                        .map_err(|_| "invalid character origin")?,
                );
                Ok(Some((x, y, advance)))
            })?
        } else {
            None
        };
        geometry.push(geo);
        let f = fold(&obj.text, false);
        counts.push(f.units.len());
        units.extend(f.units);
    }
    let target = fold(replacement, false);
    let max = target.units.len().min(units.len());
    let mut prefix = 0;
    while prefix < max && target.units[prefix] == units[prefix] {
        prefix += 1;
    }
    let ws = whitespace_clamp(
        text(field(edit, "oldText")).unwrap_or(""),
        text(field(edit, "newText")).unwrap_or(""),
    );
    let old = fold(text(field(edit, "oldText")).unwrap_or(""), false);
    let offset = ws.and_then(|_| {
        if units.len() == old.units.len() {
            Some(0)
        } else {
            index_of(&units, &old.units)
        }
    });
    if let (Some((Some(head), _)), Some(off)) = (ws, offset) {
        prefix = prefix.min((off as isize + head).max(0) as usize);
    }
    let mut suffix = 0;
    while suffix < max - prefix
        && target.units[target.units.len() - 1 - suffix] == units[units.len() - 1 - suffix]
    {
        suffix += 1;
    }
    if let (Some((_, Some(tail))), Some(off)) = (ws, offset) {
        suffix = suffix.min(
            (units.len() as isize - off as isize - old.units.len() as isize + tail).max(0) as usize,
        );
    }
    let shift = units.len() as isize - target.units.len() as isize;
    let mut result = Vec::new();
    let mut a = 0usize;
    let mut prev_end = 0usize;
    let text_units: Vec<u16> = replacement.encode_utf16().collect();
    for (i, obj) in sorted.into_iter().enumerate() {
        let b = a + counts[i];
        let in_prefix = b <= prefix;
        let in_suffix = a >= units.len() - suffix;
        if let Some((x, y, advance)) = geometry[i].filter(|_| b > a && (in_prefix || in_suffix)) {
            let first = if in_prefix {
                a as isize
            } else {
                a as isize - shift
            };
            let last = if in_prefix {
                b as isize
            } else {
                b as isize - shift
            };
            if first >= 0 && last > first && (last as usize) <= target.end.len() {
                let start = target.start[first as usize];
                let end = target.end[last as usize - 1];
                if start >= prev_end
                    && end > start
                    && !text_units[start..end].contains(&(b'\n' as u16))
                {
                    let mut chosen = color;
                    let mut eligible = true;
                    if color.is_none() {
                        if let Some(styles) = styles {
                            chosen = styles
                                .get(start)
                                .and_then(|v| v.as_ref())
                                .and_then(|s| s.color);
                            for k in start..end {
                                let s = styles.get(k).and_then(|v| v.as_ref());
                                if s.is_some_and(RunStyle::beyond_color)
                                    || s.and_then(|s| s.color) != chosen
                                {
                                    eligible = false;
                                    break;
                                }
                            }
                        }
                    }
                    if eligible {
                        result.push(Keep {
                            src: obj,
                            start,
                            end,
                            advance,
                            x,
                            y,
                            color: chosen,
                        });
                        prev_end = end;
                    }
                }
            }
        }
        a = b;
    }
    Ok((!result.is_empty()).then_some(result))
}
