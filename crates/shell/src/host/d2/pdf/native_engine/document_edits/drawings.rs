//! Native policy for PDF drawings, note threads and raster stamps.
use super::session::Session;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use shell_contract::IpcValue;
use std::collections::HashMap;

fn field<'a>(value: &'a IpcValue, key: &str) -> Option<&'a IpcValue> {
    match value {
        IpcValue::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}
fn list(value: Option<&IpcValue>) -> &[IpcValue] {
    match value {
        Some(IpcValue::Array(v)) => v,
        _ => &[],
    }
}
fn text(value: Option<&IpcValue>) -> Option<&str> {
    match value {
        Some(IpcValue::String(v)) => Some(v),
        _ => None,
    }
}
fn number(value: Option<&IpcValue>) -> Option<f64> {
    match value {
        Some(IpcValue::Number(v)) => Some(*v),
        _ => None,
    }
}
fn numbers<const N: usize>(value: Option<&IpcValue>) -> Result<[f64; N], String> {
    let values = list(value);
    if values.len() != N {
        return Err("invalid PDF drawing coordinates".into());
    }
    let mut result = [0.0; N];
    for (out, value) in result.iter_mut().zip(values) {
        *out = number(Some(value))
            .filter(|v| v.is_finite())
            .ok_or("invalid PDF drawing coordinates")?;
    }
    Ok(result)
}
fn page_index(value: &IpcValue, count: usize) -> Option<usize> {
    number(field(value, "pageIndex"))
        .filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0 && *n < count as f64)
        .map(|n| n as usize)
}
fn rounded(value: f64) -> f64 {
    jscompat::math_round(value * 100.0) / 100.0
}
fn n(value: f64) -> String {
    jscompat::number_to_string(rounded(value))
}
fn raw(value: f64) -> String {
    jscompat::number_to_string(value)
}
fn handle(session: &mut Session, command: Value) -> Result<u64, String> {
    session
        .exchange(command)?
        .as_u64()
        .ok_or_else(|| "invalid PDF object handle".into())
}
fn read(
    session: &mut Session,
    target: u64,
    key: &str,
    resolve: bool,
) -> Result<Option<u64>, String> {
    let value =
        session.exchange(json!({"op":"read","target":target,"key":key,"resolve":resolve}))?;
    if value.is_null() {
        Ok(None)
    } else {
        value
            .as_u64()
            .map(Some)
            .ok_or_else(|| "invalid PDF object handle".into())
    }
}
fn optional(session: &mut Session, command: Value) -> Result<Option<u64>, String> {
    let value = session.exchange(command)?;
    if value.is_null() {
        Ok(None)
    } else {
        value
            .as_u64()
            .map(Some)
            .ok_or_else(|| "invalid PDF object handle".into())
    }
}
fn inspect(session: &mut Session, target: u64) -> Result<Value, String> {
    session.exchange(json!({"op":"inspect","target":target}))
}
fn stream_bytes(stream: &str) -> Vec<u8> {
    stream.encode_utf16().map(|unit| unit as u8).collect()
}
fn form(
    session: &mut Session,
    rect: [f64; 4],
    stream: &str,
    resources: Option<Value>,
) -> Result<u64, String> {
    let mut dict =
        json!({"Type":{"name":"XObject"},"Subtype":{"name":"Form"},"BBox":{"array":rect}});
    if let Some(resources) = resources {
        dict["Resources"] = resources;
    }
    let bytes = stream_bytes(stream);
    handle(
        session,
        json!({"op":"stream","register":true,"bytesBase64":STANDARD.encode(bytes),"dict":dict}),
    )
}
fn append(session: &mut Session, page: usize, dict: Value) -> Result<u64, String> {
    let annotation = handle(
        session,
        json!({"op":"make","register":true,"value":{"dict":dict}}),
    )?;
    let node = handle(session, json!({"op":"page","index":page}))?;
    let existing = optional(
        session,
        json!({"op":"readMaybe","target":node,"key":"Annots","type":"array"}),
    )?;
    if let Some(array) = existing {
        session.exchange(json!({"op":"push","target":array,"value":{"ref":annotation}}))?;
    } else {
        session.exchange(
            json!({"op":"set","target":node,"key":"Annots","value":{"array":[{"ref":annotation}]}}),
        )?;
    }
    Ok(annotation)
}
fn page_ref(session: &mut Session, page: usize) -> Result<u64, String> {
    handle(session, json!({"op":"pageRef","index":page}))
}
fn signature(dict: &mut Value, drawing: &IpcValue) {
    if let Some(name) = text(field(drawing, "formFieldName")).filter(|s| !s.is_empty()) {
        dict["GenOfficeFormField"] = json!({"text":name});
        dict["Contents"] = json!({"text":format!("GenOffice visual signature field: {name}")});
    }
}
fn literal(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}
fn percent_non_ascii(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            let mut buf = [0; 4];
            for byte in ch.encode_utf8(&mut buf).as_bytes() {
                write!(out, "%{byte:02X}").expect("writing to a string cannot fail");
            }
        }
    }
    out
}
fn note_appearance(rect: [f64; 4], color: [f64; 3]) -> String {
    let [x0, y0, x1, y1] = rect;
    let [r, g, b] = color;
    let sx = (x1 - x0) / 20.0;
    let sy = (y1 - y0) / 18.0;
    let p = |x: f64, y: f64| format!("{} {}", n(x0 + x * sx), n(y0 + y * sy));
    let dark = format!("{} {} {}", n(r * 0.62), n(g * 0.62), n(b * 0.62));
    let line = 0.75 * sx.min(sy);
    let mut body = Vec::new();
    for (i, (x, y)) in [
        (1.0, 4.5),
        (1.0, 0.5),
        (6.0, 4.5),
        (17.5, 4.5),
        (20.0, 8.0),
        (20.0, 15.5),
        (17.5, 18.0),
        (2.5, 18.0),
        (0.0, 15.5),
        (0.0, 8.0),
        (1.0, 4.5),
    ]
    .into_iter()
    .enumerate()
    {
        body.push(format!("{} {}", p(x, y), if i == 0 { "m" } else { "l" }));
    }
    body.push("B".into());
    let rules = [5.5, 9.0, 12.5]
        .into_iter()
        .enumerate()
        .map(|(i, y)| {
            format!(
                "{} m {} l S",
                p(3.5, y),
                p(if i == 1 { 13.0 } else { 16.5 }, y)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "/NoteRGB CS\n{dark} SCN\n{} w\n/NoteRGB cs\n{} {} {} scn\n{}\n{dark} SCN\n{} w\n{rules}",
        n(line),
        n(r),
        n(g),
        n(b),
        body.join("\n"),
        n(line * 0.7)
    )
}
fn ellipse(ops: &mut Vec<String>, rect: [f64; 4]) {
    let [x1, y1, x2, y2] = rect;
    let k = 0.5522847;
    let cx = (x1 + x2) / 2.0;
    let cy = (y1 + y2) / 2.0;
    let rx = (x2 - x1).abs() / 2.0;
    let ry = (y2 - y1).abs() / 2.0;
    ops.push(format!("{} {} m", n(cx + rx), n(cy)));
    for points in [
        [cx + rx, cy + k * ry, cx + k * rx, cy + ry, cx, cy + ry],
        [cx - k * rx, cy + ry, cx - rx, cy + k * ry, cx - rx, cy],
        [cx - rx, cy - k * ry, cx - k * rx, cy - ry, cx, cy - ry],
        [cx + k * rx, cy - ry, cx + rx, cy - k * ry, cx + rx, cy],
    ] {
        ops.push(format!(
            "{} {} {} {} {} {} c",
            n(points[0]),
            n(points[1]),
            n(points[2]),
            n(points[3]),
            n(points[4]),
            n(points[5])
        ));
    }
    ops.push("S".into());
}
fn now_ms() -> f64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as f64,
        Err(error) => -(error.duration().as_millis() as f64),
    }
}
fn date(session: &mut Session, ms: f64) -> Result<String, String> {
    session
        .exchange(json!({"op":"pdfDate","ms":ms}))?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "invalid PDF date".into())
}
fn note_rects_close(a: [f64; 4], b: [f64; 4]) -> bool {
    (a[0].min(a[2]) - b[0].min(b[2])).abs() <= 2.0 && (a[1].max(a[3]) - b[1].max(b[3])).abs() <= 2.0
}
fn note_match(
    session: &mut Session,
    page: usize,
    target: &IpcValue,
    contents_key: &str,
) -> Result<Option<(u64, u64)>, String> {
    let rect = numbers::<4>(field(target, "rect"))?;
    let contents = text(field(target, contents_key)).ok_or("invalid note contents")?;
    let hint = number(field(target, "objNum"));
    let node = handle(session, json!({"op":"page","index":page}))?;
    let Some(array) = optional(
        session,
        json!({"op":"readMaybe","target":node,"key":"Annots","type":"array"}),
    )?
    else {
        return Ok(None);
    };
    let info = inspect(session, array)?;
    let size = info
        .get("size")
        .and_then(Value::as_u64)
        .ok_or("invalid annotation array")?;
    let mut first = None;
    for index in 0..size {
        let reference = handle(
            session,
            json!({"op":"arrayRead","target":array,"index":index}),
        )?;
        let identity = inspect(session, reference)?;
        if identity.get("type").and_then(Value::as_str) != Some("ref") {
            continue;
        }
        let Some(dict) = optional(
            session,
            json!({"op":"resolveMaybe","target":reference,"type":"dict"}),
        )?
        else {
            continue;
        };
        let Some(subtype) = optional(
            session,
            json!({"op":"readMaybe","target":dict,"key":"Subtype","type":"name"}),
        )?
        else {
            continue;
        };
        let name = inspect(session, subtype)?;
        if name.get("value").and_then(Value::as_str) != Some("Text") {
            continue;
        }
        let Some(rect_id) = optional(
            session,
            json!({"op":"readMaybe","target":dict,"key":"Rect","type":"array"}),
        )?
        else {
            continue;
        };
        let rect_info = inspect(session, rect_id)?;
        if rect_info.get("size").and_then(Value::as_u64) != Some(4) {
            continue;
        }
        let mut actual = [0.0; 4];
        let mut valid = true;
        for (i, value) in actual.iter_mut().enumerate() {
            let Some(id) = optional(
                session,
                json!({"op":"arrayReadMaybe","target":rect_id,"index":i,"type":"number"}),
            )?
            else {
                valid = false;
                break;
            };
            let item = inspect(session, id)?;
            let Some(num) = item.get("value").and_then(Value::as_f64) else {
                valid = false;
                break;
            };
            *value = num;
        }
        if !valid || !note_rects_close(actual, rect) {
            continue;
        }
        let value = if let Some(id) = read(session, dict, "Contents", true)? {
            let info = inspect(session, id)?;
            if info.get("type").and_then(Value::as_str) == Some("text") {
                info.get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned()
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        if value != contents {
            continue;
        }
        if identity.get("objectNumber").and_then(Value::as_f64) == hint {
            return Ok(Some((reference, dict)));
        }
        if first.is_none() {
            first = Some((reference, dict));
        }
    }
    Ok(first)
}
fn link_destination(drawing: &IpcValue, count: usize) -> Option<usize> {
    let target = number(field(drawing, "destPage")).unwrap_or(1.0) - 1.0;
    (target.is_finite() && target >= 0.0 && target.fract() == 0.0 && target < count as f64)
        .then_some(target as usize)
}
fn link(
    session: &mut Session,
    drawing: &IpcValue,
    page: usize,
    count: usize,
) -> Result<(), String> {
    let rect = numbers::<4>(field(drawing, "rect"))?.map(rounded);
    let reference = page_ref(session, page)?;
    let mut dict = json!({"Type":{"name":"Annot"},"Subtype":{"name":"Link"},"Rect":{"array":rect},"F":4,"P":{"ref":reference}});
    if let Some(url) = text(field(drawing, "url")).filter(|s| !s.is_empty()) {
        let address = if url.is_ascii() {
            url.to_owned()
        } else {
            let parsed = session.exchange(json!({"op":"urlHref","url":url}))?;
            if let Some(href) = parsed.as_str() {
                href.to_owned()
            } else {
                percent_non_ascii(url)
            }
        };
        dict["A"] = json!({"dict":{"Type":{"name":"Action"},"S":{"name":"URI"},"URI":{"literal":literal(&address)}}});
    } else {
        let Some(target) = link_destination(drawing, count) else {
            return Ok(());
        };
        let target = page_ref(session, target)?;
        dict["Dest"] = json!({"array":[{"ref":target},{"name":"XYZ"},null,null,null]});
    }
    append(session, page, dict)?;
    Ok(())
}
fn image_matrix(width: f64, height: f64, rotation: f64) -> String {
    let rot = ((rotation % 360.0) + 360.0) % 360.0;
    if rot == 90.0 {
        format!("0 {} {} 0 {} 0", n(height), n(-width), n(width))
    } else if rot == 180.0 {
        format!(
            "{} 0 0 {} {} {}",
            n(-width),
            n(-height),
            n(width),
            n(height)
        )
    } else if rot == 270.0 {
        format!("0 {} {} 0 0 {}", n(-height), n(width), n(height))
    } else {
        format!("{} 0 0 {} 0 0", n(width), n(height))
    }
}
fn image(
    session: &mut Session,
    drawing: &IpcValue,
    page: usize,
    author: &str,
) -> Result<(), String> {
    let image = text(field(drawing, "image")).ok_or("invalid image stamp")?;
    let png = handle(session, json!({"op":"embedPng","image":image}))?;
    let [x1, y1, x2, y2] = numbers::<4>(field(drawing, "rect"))?;
    let w = x2 - x1;
    let h = y2 - y1;
    let rot = session
        .exchange(json!({"op":"rotation","index":page}))?
        .as_f64()
        .ok_or("invalid PDF page rotation")?;
    let matrix = image_matrix(w, h, rot);
    let ap = form(
        session,
        [0.0, 0.0, rounded(w), rounded(h)],
        &format!("q {matrix} cm /Im0 Do Q"),
        Some(json!({"dict":{"XObject":{"dict":{"Im0":{"ref":png}}}}})),
    )?;
    let pref = page_ref(session, page)?;
    let mut dict = json!({"Type":{"name":"Annot"},"Subtype":{"name":"Stamp"},"Rect":{"array":[rounded(x1),rounded(y1),rounded(x2),rounded(y2)]},"F":4,"P":{"ref":pref},"T":{"text":author},"AP":{"dict":{"N":{"ref":ap}}}});
    signature(&mut dict, drawing);
    append(session, page, dict)?;
    Ok(())
}
fn note(
    session: &mut Session,
    drawing: &IpcValue,
    page: usize,
    author: &str,
    notes: &mut HashMap<String, u64>,
) -> Result<(), String> {
    let [x, y] = numbers::<2>(field(drawing, "at"))?;
    let color = numbers::<3>(field(drawing, "color"))?;
    let rect = [x, y - 18.0, x + 20.0, y];
    let ap = form(
        session,
        rect,
        &note_appearance(rect, color),
        Some(json!({"dict":{"ColorSpace":{"dict":{"NoteRGB":{"name":"DeviceRGB"}}}}})),
    )?;
    let pref = page_ref(session, page)?;
    let time = date(
        session,
        number(field(drawing, "createdMs")).unwrap_or_else(now_ms),
    )?;
    let contents = text(field(drawing, "contents")).ok_or("invalid note contents")?;
    let name = text(field(drawing, "author"))
        .filter(|s| !s.is_empty())
        .unwrap_or(author);
    let mut dict = json!({"Type":{"name":"Annot"},"Subtype":{"name":"Text"},"Rect":{"array":rect.map(rounded)},"Name":{"name":"Comment"},"C":{"array":color},"F":4,"P":{"ref":pref},"AP":{"dict":{"N":{"ref":ap}}},"Contents":{"text":contents},"T":{"text":name},"CreationDate":{"literal":time},"M":{"literal":time}});
    let parent =
        if let Some(local) = text(field(drawing, "replyToLocalId")).filter(|s| !s.is_empty()) {
            notes.get(local).copied()
        } else if let Some(target) = field(drawing, "replyToSaved")
            .filter(|v| !matches!(v, IpcValue::Null | IpcValue::Undefined))
        {
            note_match(session, page, target, "contents")?.map(|(reference, _)| reference)
        } else {
            None
        };
    if let Some(parent) = parent {
        dict["IRT"] = json!({"ref":parent});
        dict["RT"] = json!({"name":"R"});
    }
    let reference = append(session, page, dict)?;
    if let Some(id) = text(field(drawing, "localId")).filter(|s| !s.is_empty()) {
        notes.insert(id.to_owned(), reference);
    }
    Ok(())
}
fn shape(
    session: &mut Session,
    drawing: &IpcValue,
    page: usize,
    kind: &str,
    author: &str,
    font: Option<u64>,
) -> Result<(), String> {
    let color = numbers::<3>(field(drawing, "color"))?;
    let [r, g, b] = color;
    let pref = page_ref(session, page)?;
    if kind == "caret" {
        let [x, y] = numbers::<2>(field(drawing, "point"))?;
        let rect = [
            rounded(x - 10.0),
            rounded(y - 10.0),
            rounded(x + 10.0),
            rounded(y + 10.0),
        ];
        let stream = format!(
            "{} {} {} rg\n{} {} m {} {} l {} {} l f",
            raw(r),
            raw(g),
            raw(b),
            n(x),
            n(y),
            n(x - 8.0),
            n(y - 6.0),
            n(x + 8.0),
            n(y - 6.0)
        );
        let ap = form(session, rect, &stream, None)?;
        append(
            session,
            page,
            json!({"Type":{"name":"Annot"},"Subtype":{"name":"Caret"},"Rect":{"array":rect},"F":4,"P":{"ref":pref},"C":{"array":color},"T":{"text":author},"AP":{"dict":{"N":{"ref":ap}}}}),
        )?;
        return Ok(());
    }
    let width = number(field(drawing, "width"))
        .filter(|v| v.is_finite())
        .ok_or("invalid drawing width")?;
    let mut ops = vec![format!(
        "{} w 1 J 1 j {} {} {} RG",
        n(width),
        raw(r),
        raw(g),
        raw(b)
    )];
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    let subtype;
    if kind == "polygon" {
        let vertices = list(field(drawing, "vertices"));
        if vertices.is_empty() {
            return Err("invalid polygon vertices".into());
        }
        let mut flat = Vec::new();
        for (i, vertex) in vertices.iter().enumerate() {
            let [x, y] = numbers::<2>(Some(vertex))?;
            xs.push(x);
            ys.push(y);
            flat.extend([rounded(x), rounded(y)]);
            ops.push(format!(
                "{} {} {}",
                n(x),
                n(y),
                if i == 0 { "m" } else { "l" }
            ));
        }
        ops.push("h S".into());
        let rect = [
            xs.iter().copied().fold(f64::INFINITY, f64::min),
            ys.iter().copied().fold(f64::INFINITY, f64::min),
            xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            ys.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        ]
        .map(rounded);
        let ap = form(session, rect, &ops.join("\n"), None)?;
        append(
            session,
            page,
            json!({"Type":{"name":"Annot"},"Subtype":{"name":"Polygon"},"Rect":{"array":rect},"Vertices":{"array":flat},"F":4,"P":{"ref":pref},"C":{"array":color},"BS":{"dict":{"W":width}},"AP":{"dict":{"N":{"ref":ap}}},"T":{"text":author}}),
        )?;
        return Ok(());
    } else if kind == "ink" {
        subtype = "Ink";
        for path in list(field(drawing, "paths")) {
            let path = list(Some(path));
            if path.len() < 4 {
                continue;
            }
            if !path.len().is_multiple_of(2) {
                return Err("invalid ink path".into());
            }
            let vals = path
                .iter()
                .map(|v| {
                    number(Some(v))
                        .filter(|v| v.is_finite())
                        .ok_or_else(|| "invalid ink path".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            ops.push(format!("{} {} m", n(vals[0]), n(vals[1])));
            for p in vals.chunks_exact(2).skip(1) {
                ops.push(format!("{} {} l", n(p[0]), n(p[1])));
            }
            ops.push("S".into());
            for p in vals.chunks_exact(2) {
                xs.push(p[0]);
                ys.push(p[1]);
            }
        }
    } else if kind == "rect" || kind == "ellipse" {
        let rect = numbers::<4>(field(drawing, "rect"))?;
        let [x1, y1, x2, y2] = rect;
        subtype = if kind == "rect" { "Square" } else { "Circle" };
        if kind == "rect" {
            ops.push(format!(
                "{} {} {} {} re S",
                n(x1),
                n(y1),
                n(x2 - x1),
                n(y2 - y1)
            ))
        } else {
            ellipse(&mut ops, rect)
        }
        xs.extend([x1, x2]);
        ys.extend([y1, y2]);
    } else if matches!(kind, "line" | "arrow" | "measure") {
        subtype = "Line";
        let [fx, fy] = numbers::<2>(field(drawing, "from"))?;
        let [tx, ty] = numbers::<2>(field(drawing, "to"))?;
        ops.push(format!("{} {} m {} {} l S", n(fx), n(fy), n(tx), n(ty)));
        xs.extend([fx, tx]);
        ys.extend([fy, ty]);
        let ang = (ty - fy).atan2(tx - fx);
        if kind == "measure" {
            let label = text(field(drawing, "label")).ok_or("invalid measurement label")?;
            let mx = (fx + tx) / 2.0;
            let my = (fy + ty) / 2.0;
            let tilt = if ang.abs() > std::f64::consts::FRAC_PI_2 {
                ang + std::f64::consts::PI
            } else {
                ang
            };
            let (sin, cos) = tilt.sin_cos();
            ops.push(format!("BT {} {} {} rg /F1 9 Tf", raw(r), raw(g), raw(b)));
            ops.push(format!(
                "{} {} {} {} {} {} Tm",
                n(cos),
                n(sin),
                n(-sin),
                n(cos),
                n(mx),
                n(my + 6.0)
            ));
            let escaped = label
                .replace('\\', "\\\\")
                .replace('(', "\\(")
                .replace(')', "\\)");
            ops.push(format!("({escaped}) Tj ET"));
            let half = label.encode_utf16().count() as f64 * 4.5 / 2.0;
            xs.extend([mx + cos * half, mx - cos * half]);
            ys.extend([my + 6.0 + sin * half, my + 6.0 - sin * half]);
        }
        if kind == "arrow" {
            let len = 9.0_f64.max(width * 4.5);
            for off in [-0.45, 0.45] {
                let hx = tx - len * (ang + off).cos();
                let hy = ty - len * (ang + off).sin();
                ops.push(format!("{} {} m {} {} l S", n(tx), n(ty), n(hx), n(hy)));
                xs.push(hx);
                ys.push(hy);
            }
        }
    } else {
        return Err("invalid drawing kind".into());
    }
    if xs.is_empty() {
        return Err("drawing contains no points".into());
    }
    let pad = width + 2.0;
    let rect = [
        xs.iter().copied().fold(f64::INFINITY, f64::min) - pad,
        ys.iter().copied().fold(f64::INFINITY, f64::min) - pad,
        xs.iter().copied().fold(f64::NEG_INFINITY, f64::max) + pad,
        ys.iter().copied().fold(f64::NEG_INFINITY, f64::max) + pad,
    ];
    let resources = if kind == "measure" {
        Some(
            json!({"dict":{"Font":{"dict":{"F1":{"ref":font.ok_or("missing measurement font")?}}}}}),
        )
    } else {
        None
    };
    let ap = form(session, rect, &ops.join("\n"), resources)?;
    let mut dict = json!({"Type":{"name":"Annot"},"Subtype":{"name":subtype},"Rect":{"array":rect},"C":{"array":color},"F":4,"P":{"ref":pref},"BS":{"dict":{"W":width}},"AP":{"dict":{"N":{"ref":ap}}},"T":{"text":author}});
    if kind == "ink" {
        let paths = list(field(drawing, "paths"))
            .iter()
            .map(|v| {
                list(Some(v))
                    .iter()
                    .map(|n| number(Some(n)).ok_or_else(|| "invalid ink path".to_owned()))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|v| json!({"array":v}))
            })
            .collect::<Result<Vec<_>, _>>()?;
        dict["InkList"] = json!({"array":paths});
        signature(&mut dict, drawing);
    }
    if matches!(kind, "line" | "arrow" | "measure") {
        let [fx, fy] = numbers::<2>(field(drawing, "from"))?;
        let [tx, ty] = numbers::<2>(field(drawing, "to"))?;
        dict["L"] = json!({"array":[fx,fy,tx,ty]});
    }
    if kind == "measure" {
        dict["Contents"] =
            json!({"text":text(field(drawing,"label")).ok_or("invalid measurement label")?});
    }
    append(session, page, dict)?;
    Ok(())
}

pub(super) fn apply(
    session: &mut Session,
    request: &IpcValue,
    page_count: usize,
    author: &str,
    _modified_at: f64,
) -> Result<(), String> {
    let drawings = list(field(request, "drawings"));
    let font = if drawings
        .iter()
        .any(|d| text(field(d, "kind")) == Some("measure"))
    {
        Some(handle(session, json!({"op":"embedHelvetica"}))?)
    } else {
        None
    };
    let mut notes = HashMap::new();
    for drawing in drawings {
        let Some(page) = page_index(drawing, page_count) else {
            continue;
        };
        let kind = text(field(drawing, "kind")).ok_or("invalid drawing kind")?;
        match kind {
            "image" => image(session, drawing, page, author)?,
            "link" => link(session, drawing, page, page_count)?,
            "note" => note(session, drawing, page, author, &mut notes)?,
            _ => shape(session, drawing, page, kind, author, font)?,
        }
    }
    for edit in list(field(request, "noteEdits")) {
        let Some(page) = page_index(edit, page_count) else {
            continue;
        };
        let Some((_, dict)) = note_match(session, page, edit, "oldContents")? else {
            continue;
        };
        let contents = text(field(edit, "contents")).ok_or("invalid note contents")?;
        let time = date(session, now_ms())?;
        session.exchange(
            json!({"op":"set","target":dict,"key":"Contents","value":{"text":contents}}),
        )?;
        session.exchange(json!({"op":"set","target":dict,"key":"M","value":{"literal":time}}))?;
    }
    super::stamps::apply(session, list(field(request, "stamps")), page_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn note_icon_has_device_rgb_and_bubble_geometry() {
        let stream = note_appearance([10.0, 2.0, 30.0, 20.0], [1.0, 0.9, 0.3]);
        assert!(stream.contains("/NoteRGB cs"));
        assert!(stream.contains("11 6.5 m"));
        assert!(stream.ends_with("13.5 14.5 m 26.5 14.5 l S"));
    }
    #[test]
    fn ellipse_uses_four_cubic_segments() {
        let mut ops = Vec::new();
        ellipse(&mut ops, [0.0, 0.0, 20.0, 10.0]);
        assert_eq!(ops.iter().filter(|op| op.ends_with(" c")).count(), 4);
        assert_eq!(ops.last().unwrap(), "S");
    }
    #[test]
    fn literal_escapes_pdf_string_delimiters() {
        assert_eq!(literal("a(b)\\c\n"), "a\\(b\\)\\\\c\\n");
    }
    #[test]
    fn note_identity_uses_top_left_not_resized_icon_extent() {
        assert!(note_rects_close(
            [10.0, 2.0, 30.0, 20.0],
            [11.0, -80.0, 80.0, 22.0]
        ));
        assert!(!note_rects_close(
            [10.0, 2.0, 30.0, 20.0],
            [12.1, -80.0, 80.0, 22.0]
        ));
    }
    #[test]
    fn image_signature_counters_page_rotation() {
        assert_eq!(image_matrix(20.0, 10.0, 0.0), "20 0 0 10 0 0");
        assert_eq!(image_matrix(20.0, 10.0, 90.0), "0 10 -20 0 20 0");
        assert_eq!(image_matrix(20.0, 10.0, 180.0), "-20 0 0 -10 20 10");
        assert_eq!(image_matrix(20.0, 10.0, -90.0), "0 -10 20 0 0 10");
    }
    #[test]
    fn measure_stream_uses_pdf_lib_utf16_low_bytes() {
        assert_eq!(stream_bytes("A한😀"), vec![65, 0x5c, 0x3d, 0x00]);
    }
    #[test]
    fn visual_signature_keeps_the_legacy_prefix() {
        let drawing = IpcValue::Object(vec![(
            "formFieldName".into(),
            IpcValue::String("sig".into()),
        )]);
        let mut dict = json!({});
        signature(&mut dict, &drawing);
        assert_eq!(
            dict["Contents"],
            json!({"text":"GenOffice visual signature field: sig"})
        );
        assert_eq!(dict["GenOfficeFormField"], json!({"text":"sig"}));
    }
    #[test]
    fn missing_or_out_of_range_page_is_not_written() {
        let drawing = IpcValue::Object(vec![("pageIndex".into(), IpcValue::Number(3.0))]);
        assert_eq!(page_index(&drawing, 3), None);
        assert_eq!(page_index(&drawing, 4), Some(3));
    }
    #[test]
    fn link_destinations_are_one_based_and_invalid_targets_are_skipped() {
        let drawing = IpcValue::Object(vec![("destPage".into(), IpcValue::Number(2.0))]);
        assert_eq!(link_destination(&drawing, 2), Some(1));
        assert_eq!(link_destination(&drawing, 1), None);
        assert_eq!(link_destination(&IpcValue::Object(vec![]), 1), Some(0));
    }
    #[test]
    fn invalid_unicode_urls_percent_encode_only_non_ascii_bytes() {
        assert_eq!(
            percent_non_ascii("bad 한(😀)"),
            "bad %ED%95%9C(%F0%9F%98%80)"
        );
    }
}
