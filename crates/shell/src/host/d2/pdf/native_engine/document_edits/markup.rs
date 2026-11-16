//! Markup validation and subject rules, before constructing PDF annotations.
use shell_contract::IpcValue;

fn field<'a>(v: &'a IpcValue, name: &str) -> Option<&'a IpcValue> {
    match v {
        IpcValue::Object(entries) => entries.iter().find(|(k, _)| k == name).map(|(_, v)| v),
        _ => None,
    }
}
fn finite(v: &IpcValue) -> bool {
    matches!(v,IpcValue::Number(n) if n.is_finite())
}
pub(super) fn valid_markup(markup: &IpcValue) -> bool {
    let Some(IpcValue::Array(color)) = field(markup, "color") else {
        return false;
    };
    if color.len() != 3
        || !color
            .iter()
            .all(|v| matches!(v,IpcValue::Number(n) if n.is_finite() && *n>=0.0 && *n<=1.0))
    {
        return false;
    }
    let Some(IpcValue::Array(quads)) = field(markup, "quads") else {
        return false;
    };
    !quads.is_empty()
        && quads
            .iter()
            .all(|q| matches!(q,IpcValue::Array(v) if v.len()==8 && v.iter().all(finite)))
}
pub(super) fn subject(markup: &IpcValue) -> Option<String> {
    let Some(IpcValue::String(text)) = field(markup, "subject") else {
        return None;
    };
    let value: String = jscompat::trim(text).chars().take(200).collect();
    (!value.is_empty()).then_some(value)
}

/// The PDF appearance stream is constructed natively; the codec only stores it.
pub(super) fn appearance(
    kind: &str,
    color: [f64; 3],
    quads: &[[f64; 8]],
    rotation: f64,
) -> ([f64; 4], String) {
    fn n(x: f64) -> String {
        jscompat::number_to_string(jscompat::math_round(x * 100.0) / 100.0)
    }
    let mut rect = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for q in quads {
        for i in 0..4 {
            rect[0] = rect[0].min(q[i * 2]);
            rect[1] = rect[1].min(q[i * 2 + 1]);
            rect[2] = rect[2].max(q[i * 2]);
            rect[3] = rect[3].max(q[i * 2 + 1]);
        }
    }
    let rgb = format!(
        "{} {} {}",
        jscompat::number_to_string(color[0]),
        jscompat::number_to_string(color[1]),
        jscompat::number_to_string(color[2])
    );
    let mut ops = Vec::new();
    if kind == "highlight" {
        ops.push("/GsM gs".to_owned());
        ops.push(format!("{rgb} rg"));
    } else {
        ops.push(format!("{rgb} RG"));
    }
    let rot = ((rotation % 360.0) + 360.0) % 360.0;
    for q in quads {
        let x0 = [q[0], q[2], q[4], q[6]]
            .into_iter()
            .fold(f64::INFINITY, f64::min);
        let x1 = [q[0], q[2], q[4], q[6]]
            .into_iter()
            .fold(f64::NEG_INFINITY, f64::max);
        let y0 = [q[1], q[3], q[5], q[7]]
            .into_iter()
            .fold(f64::INFINITY, f64::min);
        let y1 = [q[1], q[3], q[5], q[7]]
            .into_iter()
            .fold(f64::NEG_INFINITY, f64::max);
        if kind == "highlight" {
            ops.push(format!(
                "{} {} {} {} re f",
                n(x0),
                n(y0),
                n(x1 - x0),
                n(y1 - y0)
            ));
            continue;
        }
        let h = if rot % 180.0 == 0.0 { y1 - y0 } else { x1 - x0 };
        ops.push(format!(
            "{} w",
            jscompat::number_to_string((jscompat::math_round(h * 0.06 * 100.0) / 100.0).max(0.8))
        ));
        let t = if kind == "underline" || kind == "squiggly" {
            0.08
        } else {
            0.46
        };
        if kind == "squiggly" {
            let (ax, ay, dx, dy, px, py, length) = if rot == 90.0 {
                (x1 - h * t, y0, 0.0, 1.0, -1.0, 0.0, y1 - y0)
            } else if rot == 270.0 {
                (x0 + h * t, y0, 0.0, 1.0, 1.0, 0.0, y1 - y0)
            } else {
                (
                    x0,
                    if rot == 180.0 { y1 - h * t } else { y0 + h * t },
                    1.0,
                    0.0,
                    0.0,
                    if rot == 180.0 { -1.0 } else { 1.0 },
                    x1 - x0,
                )
            };
            let at = |d: f64, off: f64| {
                format!(
                    "{} {}",
                    n(ax + dx * d + px * off),
                    n(ay + dy * d + py * off)
                )
            };
            ops.push(format!("{} m", at(0.0, 1.5)));
            let mut from = 0.0_f64;
            let mut side = 1.0;
            while from < length {
                let to = (from + 2.0).min(length);
                let third = (to - from) / 3.0;
                let ctrl = 1.5 + side * 2.0;
                ops.push(format!(
                    "{} {} {} c",
                    at(from + third, ctrl),
                    at(from + 2.0 * third, ctrl),
                    at(to, 1.5)
                ));
                from = to;
                side = -side;
            }
            ops.push("S".into());
        } else if rot == 90.0 || rot == 270.0 {
            let x = if rot == 90.0 { x1 - h * t } else { x0 + h * t };
            ops.push(format!("{} {} m {} {} l S", n(x), n(y0), n(x), n(y1)));
        } else {
            let y = if rot == 180.0 { y1 - h * t } else { y0 + h * t };
            ops.push(format!("{} {} m {} {} l S", n(x0), n(y), n(x1), n(y)));
        }
    }
    (rect, ops.join("\n"))
}

pub(super) fn apply(
    session: &mut super::session::Session,
    markup: &IpcValue,
    author: &str,
    page_count: usize,
) -> Result<(), String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::{Value, json};
    if !valid_markup(markup) {
        return Ok(());
    }
    let Some(IpcValue::Number(page)) = field(markup, "pageIndex") else {
        return Ok(());
    };
    if !page.is_finite() || *page < 0.0 || page.fract() != 0.0 || *page >= page_count as f64 {
        return Ok(());
    }
    let page = *page as usize;
    let Some(IpcValue::String(kind)) = field(markup, "type") else {
        return Err("invalid markup type".into());
    };
    let subtype = match kind.as_str() {
        "highlight" => "Highlight",
        "underline" => "Underline",
        "strikeout" => "StrikeOut",
        "squiggly" => "Squiggly",
        _ => return Err("invalid markup type".into()),
    };
    let Some(IpcValue::Array(color)) = field(markup, "color") else {
        unreachable!()
    };
    let values: Vec<f64> = color
        .iter()
        .map(|v| {
            if let IpcValue::Number(n) = v {
                *n
            } else {
                unreachable!()
            }
        })
        .collect();
    let color = [values[0], values[1], values[2]];
    let Some(IpcValue::Array(raw_quads)) = field(markup, "quads") else {
        unreachable!()
    };
    let quads: Vec<[f64; 8]> = raw_quads
        .iter()
        .map(|v| {
            let IpcValue::Array(v) = v else {
                unreachable!()
            };
            let mut q = [0.0; 8];
            for (i, v) in v.iter().enumerate() {
                let IpcValue::Number(n) = v else {
                    unreachable!()
                };
                q[i] = *n;
            }
            q
        })
        .collect();
    let command = |s: &mut super::session::Session, v: Value| -> Result<u64, String> {
        s.exchange(v)?
            .as_u64()
            .ok_or_else(|| "invalid PDF object handle".into())
    };
    let rotation = session
        .exchange(json!({"op":"rotation","index":page}))?
        .as_f64()
        .ok_or("invalid PDF page rotation")?;
    let (rect, stream) = appearance(kind, color, &quads, rotation);
    let resources = if kind == "highlight" {
        json!({"dict":{"ExtGState":{"dict":{"GsM":{"dict":{"Type":{"name":"ExtGState"},"BM":{"name":"Multiply"},"ca":1}}}}}})
    } else {
        json!({"dict":{}})
    };
    let ap = command(
        session,
        json!({"op":"stream","register":true,"bytesBase64":STANDARD.encode(stream),"dict":{"Type":{"name":"XObject"},"Subtype":{"name":"Form"},"BBox":{"array":rect},"Resources":resources}}),
    )?;
    let page_ref = command(session, json!({"op":"pageRef","index":page}))?;
    let flattened: Vec<f64> = quads.iter().flat_map(|q| q.iter().copied()).collect();
    let mut dict = json!({"Type":{"name":"Annot"},"Subtype":{"name":subtype},"Rect":{"array":rect},"QuadPoints":{"array":flattened},"C":{"array":color},"F":4,"T":{"text":author},"P":{"ref":page_ref},"AP":{"dict":{"N":{"ref":ap}}}});
    if let Some(value) = subject(markup) {
        dict["Subj"] = json!({"text":value});
    }
    let annot = command(
        session,
        json!({"op":"make","register":true,"value":{"dict":dict}}),
    )?;
    let node = command(session, json!({"op":"page","index":page}))?;
    let annots =
        session.exchange(json!({"op":"readMaybe","target":node,"key":"Annots","type":"array"}))?;
    if let Some(array) = annots.as_u64() {
        session.exchange(json!({"op":"push","target":array,"value":{"ref":annot}}))?;
    } else {
        session.exchange(
            json!({"op":"set","target":node,"key":"Annots","value":{"array":[{"ref":annot}]}}),
        )?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_color_or_quad_and_trims_subject() {
        let markup = IpcValue::Object(vec![
            (
                "color".into(),
                IpcValue::Array(vec![
                    IpcValue::Number(0.5),
                    IpcValue::Number(1.0),
                    IpcValue::Number(0.0),
                ]),
            ),
            (
                "quads".into(),
                IpcValue::Array(vec![IpcValue::Array(vec![IpcValue::Number(0.0); 8])]),
            ),
            (
                "subject".into(),
                IpcValue::String(format!("  {}  ", "\u{1f980}".repeat(201))),
            ),
        ]);
        assert!(valid_markup(&markup));
        assert_eq!(subject(&markup).unwrap().chars().count(), 200);
        let bad = IpcValue::Object(vec![
            (
                "color".into(),
                IpcValue::Array(vec![IpcValue::Number(f64::NAN); 3]),
            ),
            ("quads".into(), IpcValue::Array(vec![])),
        ]);
        assert!(!valid_markup(&bad));
    }
    #[test]
    fn appearance_respects_rotated_baseline() {
        let quad = [0.0, 10.0, 20.0, 10.0, 0.0, 0.0, 20.0, 0.0];
        let (bounds, stream) = appearance("underline", [1.0, 0.0, 0.0], &[quad], 90.0);
        assert_eq!(bounds, [0.0, 0.0, 20.0, 10.0]);
        assert_eq!(stream, "1 0 0 RG\n1.2 w\n18.4 0 m 18.4 10 l S");
    }
}
