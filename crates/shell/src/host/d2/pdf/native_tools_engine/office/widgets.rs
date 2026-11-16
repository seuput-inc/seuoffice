//! AcroForm text, checkbox and radio widgets as positioned glyphs.
use super::geometry::Rect;
use super::ir::PdfChar;
use super::paths::{number, object, ptr};
use super::script::{east_asian, script_of};
use crate::host::d2::pdf::pdfium_session::{PdfiumError, PdfiumSession};
use serde_json::{Value, json};
fn weight(c: char) -> f64 {
    if east_asian(script_of(c as u32)) {
        1.0
    } else if c == ' ' {
        0.4
    } else if c.is_ascii_digit() || c.is_ascii_uppercase() {
        0.6
    } else if ".,:;!'\"|()[]{}".contains(c) {
        0.3
    } else {
        0.5
    }
}
fn glyph(c: char, b: Rect, baseline: f64, size: f64, font: &str) -> PdfChar {
    let code = c as u32;
    PdfChar {
        code,
        text: c.to_string(),
        bounds: b,
        loose_box: b,
        origin_x: b.x0,
        origin_y: baseline,
        angle: 0.0,
        font_size: size,
        font_weight: 400.0,
        font_family: font.into(),
        italic: false,
        color: "000000".into(),
        hscale: None,
        invisible: false,
        is_generated: true,
        pdf_char_index: None,
        text_object_id: None,
        is_hyphen: false,
        script: script_of(code),
        highlight: None,
        underline: false,
        strike: false,
        note_ref: None,
        href: None,
    }
}
fn wrap(row: &[char], size: f64, width: f64) -> Vec<Vec<char>> {
    let mut rows = Vec::new();
    let mut start = 0;
    while start < row.len() {
        let (mut used, mut end, mut last) = (0.0, start, None);
        while end < row.len() {
            let w = weight(row[end]) * size;
            if used + w > width && end > start {
                break;
            }
            if row[end] == ' ' {
                last = Some(end);
            }
            used += w;
            end += 1;
        }
        if end < row.len()
            && let Some(space) = last.filter(|n| *n > start)
        {
            end = space + 1;
        }
        rows.push(row[start..end].to_vec());
        start = end;
    }
    rows
}
fn text_chars(bounds: Rect, value: &str, multiline: bool, requested: f64) -> Vec<PdfChar> {
    let h = bounds.height() - 4.0;
    let w = bounds.width() - 4.0;
    if h <= 0.0 || w <= 0.0 {
        return Vec::new();
    }
    let value = value.replace("\r\n", "\n").replace('\r', "\n");
    let raw: Vec<Vec<char>> = value.split('\n').map(|row| row.chars().collect()).collect();
    let multiline = multiline || raw.len() > 1;
    let mut size = if requested > 0.0 {
        requested.min(h.max(5.0))
    } else {
        h.clamp(5.0, 14.0)
    };
    let rows = if multiline {
        raw.iter()
            .flat_map(|row| {
                if row.is_empty() {
                    vec![Vec::new()]
                } else {
                    wrap(row, size, w)
                }
            })
            .collect::<Vec<_>>()
    } else {
        let natural = raw[0].iter().map(|c| weight(*c) * size).sum::<f64>();
        if natural > w {
            size = (size * w / natural).max(5.0);
        }
        raw
    };
    let first = if multiline {
        bounds.y1 - 2.0
    } else {
        (bounds.y0 + bounds.y1) / 2.0 + size * 0.93 / 2.0
    };
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if row.is_empty() {
            continue;
        }
        let baseline = first - i as f64 * size * 1.15 - size * 0.72;
        if baseline < bounds.y0 {
            break;
        }
        let mut x = bounds.x0 + 2.0;
        for c in row {
            let advance = weight(*c) * size;
            if x + advance > bounds.x1 - 2.0 + 0.01 {
                break;
            }
            let b = Rect {
                x0: x,
                x1: x + advance,
                y0: baseline - size * 0.21,
                y1: baseline + size * 0.72,
            };
            let mut ch = glyph(*c, b, baseline, size, "Helvetica");
            if jscompat::trim(&ch.text).is_empty() {
                ch.text = " ".into();
            }
            ch.loose_box.y0 = baseline - size * 0.25;
            ch.loose_box.y1 = baseline + size * 0.95;
            out.push(ch);
            x += advance;
        }
    }
    out
}
fn allocated_string(
    s: &mut PdfiumSession,
    name: &str,
    args: &[Value],
) -> Result<String, PdfiumError> {
    let mut query = args.to_vec();
    query.extend([json!(0), json!(0)]);
    let len = number(s, name, &query)? as usize;
    if len <= 2 {
        return Ok(String::new());
    }
    let buffer = s.alloc(len)?;
    let result = (|| {
        let mut read = args.to_vec();
        read.extend([ptr(buffer, 0), json!(len)]);
        number(s, name, &read)?;
        let bytes = s.read(buffer, 0, len - 2)?;
        let u: Vec<_> = bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        Ok(String::from_utf16_lossy(&u))
    })();
    let free = s.free(buffer);
    match (result, free) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(())) => Ok(v),
    }
}
fn annot_string(s: &mut PdfiumSession, annot: u64, key: &str) -> Result<String, PdfiumError> {
    if !s.supports("FPDFAnnot_GetStringValue")? {
        return Ok(String::new());
    }
    let b = s.alloc(key.len() + 1)?;
    let result = (|| {
        let mut k = key.as_bytes().to_vec();
        k.push(0);
        s.write(b, 0, &k)?;
        allocated_string(s, "FPDFAnnot_GetStringValue", &[ptr(annot, 0), ptr(b, 0)])
    })();
    let free = s.free(b);
    match (result, free) {
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
        (Ok(v), Ok(())) => Ok(v),
    }
}
fn one(
    s: &mut PdfiumSession,
    form: u64,
    annot: u64,
    scratch: u64,
) -> Result<(Vec<PdfChar>, Vec<PdfChar>), PdfiumError> {
    let empty = || (Vec::new(), Vec::new());
    if number(s, "FPDFAnnot_GetSubtype", &[ptr(annot, 0)])? != 20.0 {
        return Ok(empty());
    }
    let kind = number(
        s,
        "FPDFAnnot_GetFormFieldType",
        &[ptr(form, 0), ptr(annot, 0)],
    )? as i32;
    if !matches!(kind, 2 | 3 | 6)
        || number(s, "FPDFAnnot_GetRect", &[ptr(annot, 0), ptr(scratch, 0)])? == 0.0
    {
        return Ok(empty());
    }
    let bytes: [u8; 16] = s
        .read(scratch, 0, 16)?
        .try_into()
        .map_err(|_| PdfiumError::from("invalid widget rectangle".to_owned()))?;
    let f =
        |i: usize| f32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as f64;
    let bounds = Rect {
        x0: f(0).min(f(8)),
        x1: f(0).max(f(8)),
        y0: f(4).min(f(12)),
        y1: f(4).max(f(12)),
    };
    if kind == 6 {
        let flags = if s.supports("FPDFAnnot_GetFormFieldFlags")? {
            number(
                s,
                "FPDFAnnot_GetFormFieldFlags",
                &[ptr(form, 0), ptr(annot, 0)],
            )? as u32
        } else {
            0
        };
        if flags & (1 << 13) != 0 {
            return Ok(empty());
        }
        let value = if s.supports("FPDFAnnot_GetFormFieldValue")? {
            let value = allocated_string(
                s,
                "FPDFAnnot_GetFormFieldValue",
                &[ptr(form, 0), ptr(annot, 0)],
            )?;
            if value.is_empty() {
                annot_string(s, annot, "V")?
            } else {
                value
            }
        } else {
            annot_string(s, annot, "V")?
        };
        let size = if s.supports("FPDFAnnot_GetFontSize")? {
            s.write(scratch, 0, &[0; 4])?;
            if number(
                s,
                "FPDFAnnot_GetFontSize",
                &[ptr(form, 0), ptr(annot, 0), ptr(scratch, 0)],
            )? != 0.0
            {
                let bytes: [u8; 4] = s
                    .read(scratch, 0, 4)?
                    .try_into()
                    .map_err(|_| PdfiumError::from("invalid widget font size".to_owned()))?;
                let size = f32::from_le_bytes(bytes) as f64;
                if size.is_finite() && size > 0.0 {
                    size
                } else {
                    0.0
                }
            } else {
                0.0
            }
        } else {
            0.0
        };
        return Ok((
            Vec::new(),
            text_chars(bounds, &value, flags & (1 << 12) != 0, size),
        ));
    }
    let side = bounds.width().max(bounds.height());
    if !(4.0..=24.0).contains(&side) {
        return Ok(empty());
    }
    let appearance = annot_string(s, annot, "AS")?;
    let checked = number(s, "FPDFAnnot_IsChecked", &[ptr(form, 0), ptr(annot, 0)])? == 1.0
        || (!appearance.is_empty() && appearance != "Off");
    let c = match (kind, checked) {
        (3, true) => '◉',
        (3, false) => '○',
        (_, true) => '☒',
        (_, false) => '☐',
    };
    Ok((
        vec![glyph(
            c,
            bounds,
            bounds.y0,
            side.clamp(6.0, 14.0),
            "Segoe UI Symbol",
        )],
        Vec::new(),
    ))
}
pub(super) fn read(
    s: &mut PdfiumSession,
    doc: u64,
    page: u64,
) -> Result<(Vec<PdfChar>, Vec<PdfChar>), PdfiumError> {
    for name in [
        "FPDFPage_GetAnnotCount",
        "FPDFPage_GetAnnot",
        "FPDFPage_CloseAnnot",
        "FPDFAnnot_GetSubtype",
        "FPDFAnnot_GetRect",
        "FPDFAnnot_GetFormFieldType",
        "FPDFAnnot_IsChecked",
        "PDFiumExt_OpenFormFillInfo",
        "PDFiumExt_InitFormFillEnvironment",
        "PDFiumExt_ExitFormFillEnvironment",
        "PDFiumExt_CloseFormFillInfo",
    ] {
        if !s.supports(name)? {
            return Ok((Vec::new(), Vec::new()));
        }
    }
    let total = number(s, "FPDFPage_GetAnnotCount", &[ptr(page, 0)])? as i32;
    if total <= 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let Some(info) = object(s, "PDFiumExt_OpenFormFillInfo", &[])? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let result = (|| {
        let Some(form) = object(
            s,
            "PDFiumExt_InitFormFillEnvironment",
            &[ptr(doc, 0), ptr(info, 0)],
        )?
        else {
            return Ok((Vec::new(), Vec::new()));
        };
        let work = (|| {
            if s.supports("FORM_OnAfterLoadPage")? {
                s.call("FORM_OnAfterLoadPage", &[ptr(page, 0), ptr(form, 0)])?;
            }
            let scratch = s.alloc(16)?;
            let result = (|| {
                let (mut glyphs, mut text) = (Vec::new(), Vec::new());
                for i in 0..total {
                    let Some(annot) = object(s, "FPDFPage_GetAnnot", &[ptr(page, 0), json!(i)])?
                    else {
                        continue;
                    };
                    let result = one(s, form, annot, scratch);
                    let close = s.call("FPDFPage_CloseAnnot", &[ptr(annot, 0)]);
                    let (g, t) = match (result, close) {
                        (Err(error), _) | (Ok(_), Err(error)) => return Err(error),
                        (Ok(value), Ok(_)) => value,
                    };
                    glyphs.extend(g);
                    text.extend(t);
                }
                Ok((glyphs, text))
            })();
            let free = s.free(scratch);
            match (result, free) {
                (Err(error), _) | (Ok(_), Err(error)) => Err(error),
                (Ok(value), Ok(())) => Ok(value),
            }
        })();
        let before = match s.supports("FORM_OnBeforeClosePage") {
            Ok(true) => s
                .call("FORM_OnBeforeClosePage", &[ptr(page, 0), ptr(form, 0)])
                .map(|_| ()),
            Ok(false) => Ok(()),
            Err(error) => Err(error),
        };
        let exit = s.call("PDFiumExt_ExitFormFillEnvironment", &[ptr(form, 0)]);
        match (work, before, exit) {
            (Err(error), _, _) | (Ok(_), Err(error), _) | (Ok(_), Ok(()), Err(error)) => Err(error),
            (Ok(value), Ok(()), Ok(_)) => Ok(value),
        }
    })();
    let close = s.call("PDFiumExt_CloseFormFillInfo", &[ptr(info, 0)]);
    match (result, close) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(_)) => Ok(value),
    }
}
pub(super) fn insert(chars: &mut Vec<PdfChar>, mut glyphs: Vec<PdfChar>, text: Vec<PdfChar>) {
    chars.extend(text);
    for mut c in glyphs.drain(..) {
        let cy = c.bounds.center_y();
        let cx = c.bounds.center_x();
        let mut best = None;
        let mut best_key = f64::INFINITY;
        let mut best_gap = f64::INFINITY;
        for (i, r) in chars.iter().enumerate() {
            if r.is_generated
                || r.bounds.y0 > cy
                || r.bounds.y1 < cy
                || super::quality::whitespace(r.code)
            {
                continue;
            }
            let overlap = r.bounds.x0 < c.bounds.x1 && r.bounds.x1 > c.bounds.x0;
            let gap = if overlap {
                0.0
            } else if r.bounds.x0 >= c.bounds.x1 {
                r.bounds.x0 - c.bounds.x1
            } else {
                c.bounds.x0 - r.bounds.x1
            };
            let key = gap + (r.bounds.center_x() - cx).abs() * 0.001;
            if key < best_key {
                best = Some(i);
                best_key = key;
                best_gap = gap;
            }
        }
        let Some(i) = best else {
            chars.push(c);
            continue;
        };
        let label = &chars[i];
        c.origin_y = label.origin_y;
        c.font_size = label.font_size;
        c.font_family = label.font_family.clone();
        let side = c.font_size;
        let mut x = c.bounds.x0;
        if best_gap < 24.0 {
            x = if label.bounds.center_x() >= cx {
                label.bounds.x0 - side - 2.0
            } else {
                label.bounds.x1 + 2.0
            };
        }
        c.bounds = Rect {
            x0: x,
            x1: x + side,
            y0: c.origin_y,
            y1: c.origin_y + side,
        };
        c.loose_box = c.bounds;
        c.origin_x = x;
        let at = if c.bounds.x0 <= label.bounds.x0 {
            i
        } else {
            i + 1
        };
        chars.insert(at, c);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn field_values_wrap_and_keep_glyphs_inside_widget() {
        let bounds = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 48.0,
            y1: 55.0,
        };
        let chars = text_chars(bounds, "Alpha Beta Gamma", true, 10.0);
        assert!(
            chars
                .iter()
                .all(|c| c.bounds.x0 >= 2.0 && c.bounds.x1 <= 46.01)
        );
        assert!(chars.iter().any(|c| c.origin_y < chars[0].origin_y));
        assert!(chars.iter().all(|c| c.is_generated));
    }
    #[test]
    fn field_text_retains_original_whitespace_code_and_natural_width() {
        let bounds = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 300.0,
            y1: 20.0,
        };
        let chars = text_chars(bounds, "X\u{a0}Y", false, 10.0);
        assert_eq!(
            chars.iter().map(|c| c.text.as_str()).collect::<String>(),
            "X Y"
        );
        assert_eq!(chars[1].code, 0xa0);
        assert!((chars[0].bounds.width() - 6.0).abs() < 1e-9);
        assert!(chars[2].bounds.x1 < 30.0);
    }

    #[test]
    fn checkbox_snaps_to_nearest_overlapping_label_in_stream_order() {
        let bounds = Rect {
            x0: 10.0,
            y0: 20.0,
            x1: 22.0,
            y1: 32.0,
        };
        let mut far = glyph(
            'F',
            Rect {
                x0: 30.0,
                y0: 20.0,
                x1: 35.0,
                y1: 30.0,
            },
            21.0,
            10.0,
            "Far",
        );
        far.is_generated = false;
        let mut near = glyph(
            'N',
            Rect {
                x0: 19.0,
                y0: 20.0,
                x1: 24.0,
                y1: 30.0,
            },
            22.0,
            9.0,
            "Near",
        );
        near.is_generated = false;
        let checkbox = glyph('☒', bounds, 20.0, 12.0, "Segoe UI Symbol");
        let mut chars = vec![far, near];
        insert(&mut chars, vec![checkbox], Vec::new());
        assert_eq!(
            chars.iter().map(|c| c.text.as_str()).collect::<String>(),
            "F☒N"
        );
        assert_eq!(chars[1].origin_y, 22.0);
        assert_eq!(chars[1].font_family, "Near");
        assert_eq!(chars[1].bounds.x0, 8.0);
    }
}
