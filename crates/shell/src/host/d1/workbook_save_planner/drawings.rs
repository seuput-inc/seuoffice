use jscompat::TryJsRegex;
use serde_json::Value;
mod cell_images;

fn marker(prefix: &str, name: &str, anchor: &Value) -> Result<String, String> {
    let field = |key: &str| -> Result<String, String> {
        anchor
            .get(key)
            .and_then(Value::as_number)
            .map(ToString::to_string)
            .ok_or_else(|| format!("Drawing anchor has no {key}."))
    };
    let (col, col_off, row, row_off) = if name == "from" {
        ("fromColumn", "fromColumnOffset", "fromRow", "fromRowOffset")
    } else {
        ("toColumn", "toColumnOffset", "toRow", "toRowOffset")
    };
    Ok(format!(
        "<{prefix}{name}><{prefix}col>{}</{prefix}col><{prefix}colOff>{}</{prefix}colOff><{prefix}row>{}</{prefix}row><{prefix}rowOff>{}</{prefix}rowOff></{prefix}{name}>",
        field(col)?,
        field(col_off)?,
        field(row)?,
        field(row_off)?
    ))
}

fn move_anchor(xml: &str, prefix: &str, kind: &str, anchor: &Value) -> Result<String, String> {
    if kind == "absoluteAnchor" {
        return Err("This visual uses an absolute anchor — moving it is not supported.".into());
    }
    let replace_marker = |input: &str, name: &str| -> Result<String, String> {
        let open = format!("<{prefix}{name}>");
        let close = format!("</{prefix}{name}>");
        let start = input.find(&open).ok_or_else(|| {
            format!("Drawing anchor has no {name} marker — moving it is not supported.")
        })?;
        let end = input[start..]
            .find(&close)
            .map(|i| start + i + close.len())
            .ok_or_else(|| {
                format!("Drawing anchor has no {name} marker — moving it is not supported.")
            })?;
        Ok(format!(
            "{}{}{}",
            &input[..start],
            marker(prefix, name, anchor)?,
            &input[end..]
        ))
    };
    let moved = replace_marker(xml, "from")?;
    if kind == "twoCellAnchor" {
        replace_marker(&moved, "to")
    } else {
        Ok(moved)
    }
}

fn anchor_spans(xml: &str) -> Result<Vec<(usize, usize, String, String)>, String> {
    let re = TryJsRegex::new(
        r"<([A-Za-z_][\w.-]*:)?(twoCellAnchor|oneCellAnchor|absoluteAnchor)\b[\s\S]*?</\1\2>",
        "g",
    )
    .map_err(|e| e.to_string())?;
    re.try_find_iter(xml)
        .map(|found| {
            let found = found.map_err(|e| e.to_string())?;
            Ok((
                found.start(),
                found.end(),
                found.get(1).unwrap_or("").to_owned(),
                found.get(2).unwrap_or("").to_owned(),
            ))
        })
        .collect()
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn solid(color: &str) -> String {
    if color == "none" {
        "<a:noFill/>".into()
    } else {
        format!(
            "<a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill>",
            color.strip_prefix('#').unwrap_or(color).to_uppercase()
        )
    }
}
fn xfrm(properties: &Value) -> String {
    let rotation = properties
        .get("rotation")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let rot = ((rotation * 60000.0 + 0.5).floor() as i64).rem_euclid(21600000);
    let mut attr = String::new();
    if rot != 0 {
        attr.push_str(&format!(" rot=\"{rot}\""));
    }
    if properties.get("flipH").and_then(Value::as_bool) == Some(true) {
        attr.push_str(" flipH=\"1\"");
    }
    if properties.get("flipV").and_then(Value::as_bool) == Some(true) {
        attr.push_str(" flipV=\"1\"");
    }
    let size = properties.get("frameSize");
    let width = size
        .and_then(|v| v.get("width"))
        .and_then(Value::as_f64)
        .map_or(0.0, |n| (n + 0.5).floor());
    let height = size
        .and_then(|v| v.get("height"))
        .and_then(Value::as_f64)
        .map_or(0.0, |n| (n + 0.5).floor());
    format!(
        "<a:xfrm{attr}><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{width}\" cy=\"{height}\"/></a:xfrm>"
    )
}
fn cnvpr(id: u64, name: &str, properties: &Value, link: Option<&str>) -> String {
    let descr = properties
        .get("altText")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| format!(" descr=\"{}\"", escape(s)))
        .unwrap_or_default();
    let open = format!("<xdr:cNvPr id=\"{id}\" name=\"{}\"{descr}", escape(name));
    match link {
        None => format!("{open}/>"),
        Some(link) => format!(
            "{open}><a:hlinkClick xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" r:id=\"{}\"/></xdr:cNvPr>",
            escape(link)
        ),
    }
}
fn shape_xml(id: u64, shape: &Value, link: Option<&str>) -> Result<String, String> {
    let is_text = shape.get("isTextBox").and_then(Value::as_bool) == Some(true);
    let name = format!("{} {id}", if is_text { "TextBox" } else { "Shape" });
    let fill = solid(
        shape
            .get("fillColor")
            .and_then(Value::as_str)
            .unwrap_or("none"),
    );
    let line = shape
        .get("lineColor")
        .and_then(Value::as_str)
        .or(if is_text { Some("#808080") } else { None })
        .map(|c| format!("<a:ln w=\"9525\">{}</a:ln>", solid(c)))
        .unwrap_or_default();
    let text = shape.get("text").and_then(Value::as_str);
    let body = if text.is_some() || is_text {
        format!(
            "<xdr:txBody><a:bodyPr wrap=\"square\" rtlCol=\"0\"/><a:lstStyle/><a:p><a:r><a:t>{}</a:t></a:r></a:p></xdr:txBody>",
            escape(text.unwrap_or(""))
        )
    } else {
        String::new()
    };
    let kind = shape
        .get("shapeType")
        .and_then(Value::as_str)
        .ok_or("A shape needs a type.")?;
    Ok(format!(
        "<xdr:sp macro=\"\" textlink=\"\"><xdr:nvSpPr>{}<xdr:cNvSpPr{}/></xdr:nvSpPr><xdr:spPr>{}<a:prstGeom prst=\"{}\"><a:avLst/></a:prstGeom>{fill}{line}</xdr:spPr>{body}</xdr:sp>",
        cnvpr(id, &name, shape, link),
        if is_text { " txBox=\"1\"" } else { "" },
        xfrm(shape),
        escape(kind)
    ))
}
fn picture_xml(id: u64, rid: &str, image: &Value, link: Option<&str>) -> String {
    let opacity = image.get("opacity").and_then(Value::as_f64);
    let blip = match opacity {
        Some(v) if v < 1.0 => format!(
            "><a:alphaModFix amt=\"{}\"/></a:blip>",
            (v * 100000.0 + 0.5).floor()
        ),
        _ => "/>".into(),
    };
    let mut crop = String::new();
    if let Some(c) = image.get("crop") {
        for (field, attr) in [("left", "l"), ("top", "t"), ("right", "r"), ("bottom", "b")] {
            let n = (c.get(field).and_then(Value::as_f64).unwrap_or(0.0) * 100000.0 + 0.5).floor();
            if n != 0.0 {
                crop.push_str(&format!(" {attr}=\"{n}\""));
            }
        }
    }
    if !crop.is_empty() {
        crop = format!("<a:srcRect{crop}/>");
    }
    format!(
        "<xdr:pic><xdr:nvPicPr>{}<xdr:cNvPicPr><a:picLocks noChangeAspect=\"1\"/></xdr:cNvPicPr></xdr:nvPicPr><xdr:blipFill><a:blip xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" r:embed=\"{}\"{blip}{crop}<a:stretch><a:fillRect/></a:stretch></xdr:blipFill><xdr:spPr>{}<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></xdr:spPr></xdr:pic>",
        cnvpr(id, &format!("Picture {id}"), image, link),
        escape(rid),
        xfrm(image)
    )
}
fn chart_frame(id: u64, rid: &str, chart: &Value) -> String {
    format!(
        "<xdr:graphicFrame macro=\"\"><xdr:nvGraphicFramePr>{}<xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/></xdr:xfrm><a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/chart\"><c:chart xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" r:id=\"{}\"/></a:graphicData></a:graphic></xdr:graphicFrame>",
        cnvpr(id, &format!("Chart {id}"), chart, None),
        escape(rid)
    )
}

fn append_anchor(
    pkg: &mut super::package::PackageEditor<'_>,
    path: &str,
    anchor: &Value,
    properties: &Value,
    inner: impl FnOnce(u64) -> Result<String, String>,
) -> Result<(), String> {
    let original = pkg.read_text(path)?;
    let re = TryJsRegex::new(r"<(?:([A-Za-z_][\w.-]*):)?wsDr(?=[\s/>])([^>]*)>", "")
        .map_err(|e| e.to_string())?;
    let root = re
        .try_find_iter(&original)
        .next()
        .transpose()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{path} is not a spreadsheet drawing part."))?;
    let name = root.get(1).map_or(String::new(), |s| format!("{s}:"));
    let close = format!("</{name}wsDr>");
    let root_text = root.as_str().to_owned();
    let mut xml = if root.get(2).unwrap_or("").trim_end().ends_with('/') {
        let at = root_text.rfind('/').ok_or("Invalid drawing root.")?;
        format!(
            "{}{}>{close}{}",
            &original[..root.start()],
            &root_text[..at],
            &original[root.end()..]
        )
    } else {
        original.as_str().to_owned()
    };
    let close_at = xml
        .rfind(&close)
        .ok_or_else(|| format!("{path} is not a spreadsheet drawing part."))?;
    let ids = TryJsRegex::new(r#"<(?:[\w.-]+:)?cNvPr [^>]*id="(\d+)""#, "g")
        .map_err(|e| e.to_string())?;
    let mut id = 1u64;
    for found in ids.try_find_iter(&xml) {
        let found = found.map_err(|e| e.to_string())?;
        if let Some(n) = found.get(1).and_then(|v| v.parse().ok()) {
            id = id.max(n);
        }
    }
    let id = id.checked_add(1).ok_or("Drawing shape ID overflow.")?;
    let edit_as = properties
        .get("editAs")
        .and_then(Value::as_str)
        .filter(|v| *v != "twoCell")
        .map(|v| format!(" editAs=\"{}\"", escape(v)))
        .unwrap_or_default();
    let xdr = if root_text.contains("xmlns:xdr=") {
        ""
    } else {
        " xmlns:xdr=\"http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing\""
    };
    let a = if root_text.contains("xmlns:a=") {
        ""
    } else {
        " xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\""
    };
    let content = format!(
        "<xdr:twoCellAnchor{edit_as}{xdr}{a}>{}{}{inner}<xdr:clientData/></xdr:twoCellAnchor>",
        marker("xdr:", "from", anchor)?,
        marker("xdr:", "to", anchor)?,
        inner = inner(id)?
    );
    xml.insert_str(close_at, &content);
    pkg.write(path, xml)
}

fn register_default(
    pkg: &mut super::package::PackageEditor<'_>,
    ext: &str,
    mime: &str,
    touched: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    let path = "[Content_Types].xml";
    let xml = pkg.read_text(path)?;
    let re = TryJsRegex::new(r#"<Default\b[^>]*\bExtension="([^"]+)"[^>]*/>"#, "g")
        .map_err(|e| e.to_string())?;
    for found in re.try_find_iter(&xml) {
        let found = found.map_err(|e| e.to_string())?;
        if found.get(1).is_some_and(|v| v.eq_ignore_ascii_case(ext)) {
            return Ok(());
        }
    }
    let at = xml
        .rfind("</Types>")
        .ok_or("[Content_Types].xml is malformed.")?;
    pkg.write(
        path,
        format!(
            "{}<Default Extension=\"{}\" ContentType=\"{}\"/>{}",
            &xml[..at],
            escape(ext),
            escape(mime),
            &xml[at..]
        ),
    )?;
    touched.insert(path.into());
    Ok(())
}

fn hyperlink(
    pkg: &mut super::package::PackageEditor<'_>,
    path: &str,
    properties: &Value,
    touched: &mut std::collections::HashSet<String>,
) -> Result<Option<String>, String> {
    let Some(url) = properties
        .get("hyperlink")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let rels = super::pivots::rels_path_for(path);
    let id = super::pivots::append_relationship(
        pkg,
        &rels,
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink",
        url,
        true,
    )?;
    touched.insert(rels);
    Ok(Some(id))
}

fn add_visuals(
    pkg: &mut super::package::PackageEditor<'_>,
    state: &mut super::state::PlannerState,
    additions: &[Value],
    touched: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    use super::pivots::{
        allocate_part_path, append_relationship, register_content_type_override, relative_target,
        rels_path_for,
    };
    use base64::Engine;
    for addition in additions {
        let sheet = addition
            .get("sheetName")
            .and_then(Value::as_str)
            .ok_or("Visual addition has no sheetName.")?;
        let sheet_path = state.path_for(pkg, sheet)?;
        let drawing = super::slicers::ensure_sheet_drawing(pkg, &sheet_path, touched)?;
        let anchor = addition
            .get("anchor")
            .ok_or("Visual addition has no anchor.")?;
        let rels = rels_path_for(&drawing);
        if let Some(chart) = addition.get("chart").filter(|v| !v.is_null()) {
            let mut chart = chart.clone();
            if let Some(path) = chart.get("pivotTablePath").and_then(Value::as_str) {
                let valid = path.starts_with("xl/pivotTables/")
                    && path.ends_with(".xml")
                    && !path["xl/pivotTables/".len()..].contains('/');
                if valid && pkg.has(path)? {
                    let xml = pkg.read_text(path)?;
                    let re = TryJsRegex::new(r#"<pivotTableDefinition\b[^>]*\bname="([^"]+)""#, "")
                        .map_err(|e| e.to_string())?;
                    if let Some(name) = re
                        .try_find_iter(&xml)
                        .next()
                        .transpose()
                        .map_err(|e| e.to_string())?
                        .and_then(|m| m.get(1).map(str::to_owned))
                    {
                        chart["pivotSourceName"] = Value::String(format!("[]{sheet}!{name}"));
                    }
                }
            }
            let path = allocate_part_path(pkg, "xl/charts/chart", ".xml")?;
            pkg.add(&path, super::charts::build_new_chart(&chart)?);
            touched.insert(path.clone());
            register_content_type_override(
                pkg,
                &path,
                "application/vnd.openxmlformats-officedocument.drawingml.chart+xml",
                touched,
            )?;
            let rid = append_relationship(
                pkg,
                &rels,
                "http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart",
                &relative_target(&drawing, &path),
                false,
            )?;
            touched.insert(rels);
            append_anchor(pkg, &drawing, anchor, &chart, |id| {
                Ok(chart_frame(id, &rid, &chart))
            })?;
        } else if let Some(shape) = addition.get("shape").filter(|v| !v.is_null()) {
            let link = hyperlink(pkg, &drawing, shape, touched)?;
            append_anchor(pkg, &drawing, anchor, shape, |id| {
                shape_xml(id, shape, link.as_deref())
            })?;
        } else if let Some(image) = addition.get("image").filter(|v| !v.is_null()) {
            let mime = image
                .get("mediaType")
                .and_then(Value::as_str)
                .ok_or("Image has no media type.")?;
            let ext = match mime {
                "image/png" => "png",
                "image/jpeg" => "jpeg",
                "image/gif" => "gif",
                _ => return Err(format!("Unsupported image media type {mime}.")),
            };
            let path = allocate_part_path(pkg, "xl/media/image", &format!(".{ext}"))?;
            let encoded = image
                .get("base64")
                .and_then(Value::as_str)
                .ok_or("Image has no bytes.")?;
            let clean: String = encoded
                .chars()
                .filter(|c| {
                    c.is_ascii_alphanumeric() || *c == '+' || *c == '/' || *c == '-' || *c == '_'
                })
                .map(|c| match c {
                    '-' => '+',
                    '_' => '/',
                    _ => c,
                })
                .collect();
            let data = base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(clean)
                .map_err(|e| e.to_string())?;
            pkg.add_binary(&path, data)?;
            touched.insert(path.clone());
            register_default(pkg, ext, mime, touched)?;
            let rid = append_relationship(
                pkg,
                &rels,
                "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image",
                &relative_target(&drawing, &path),
                false,
            )?;
            touched.insert(rels);
            let link = hyperlink(pkg, &drawing, image, touched)?;
            append_anchor(pkg, &drawing, anchor, image, |id| {
                Ok(picture_xml(id, &rid, image, link.as_deref()))
            })?;
        } else {
            return Err("A visual addition carries exactly one of chart, shape, or image.".into());
        }
        touched.insert(drawing);
    }
    Ok(())
}

fn first_span(xml: &str, pattern: &str) -> Result<Option<(usize, usize)>, String> {
    let re = TryJsRegex::new(pattern, "").map_err(|e| e.to_string())?;
    let m = re
        .try_find_iter(xml)
        .next()
        .transpose()
        .map_err(|e| e.to_string())?;
    Ok(m.map(|m| (m.start(), m.end())))
}
fn substitute(xml: &str, a: usize, b: usize, text: &str) -> String {
    format!("{}{}{}", &xml[..a], text, &xml[b..])
}
fn remove_all(xml: &str, pattern: &str) -> Result<String, String> {
    let re = TryJsRegex::new(pattern, "g").map_err(|e| e.to_string())?;
    let mut out = String::new();
    let mut at = 0;
    for m in re.try_find_iter(xml) {
        let m = m.map_err(|e| e.to_string())?;
        out.push_str(&xml[at..m.start()]);
        at = m.end();
    }
    out.push_str(&xml[at..]);
    Ok(out)
}
fn set_attr(tag: &str, name: &str, value: Option<&str>) -> Result<String, String> {
    let pattern = format!(r#"\s{name}="[^"]*""#);
    let stripped = if let Some((a, b)) = first_span(tag, &pattern)? {
        substitute(tag, a, b, "")
    } else {
        tag.to_owned()
    };
    match value {
        None => Ok(stripped),
        Some(value) => {
            let at = if stripped.ends_with("/>") {
                stripped.len() - 2
            } else {
                stripped.len() - 1
            };
            Ok(substitute(
                &stripped,
                at,
                at,
                &format!(" {name}=\"{}\"", escape(value)),
            ))
        }
    }
}
const FILL: &str =
    r"<a:(noFill|solidFill|gradFill|blipFill|pattFill|grpFill)\b(?:[^>]*/>|[^>]*>[\s\S]*?</a:\1>)";
const LINE: &str = r"<a:ln\b(?:[^>]*/>|[^>]*>[\s\S]*?</a:ln>)";
fn repaint_shape(
    xml: &str,
    prefix: &str,
    fill: Option<&str>,
    line: Option<&str>,
) -> Result<String, String> {
    if first_span(xml, &format!(r"<{prefix}sp[\s>]"))?.is_none()
        || first_span(xml, &format!(r"<{prefix}grpSp[\s>]"))?.is_some()
    {
        return Err("Only standalone shapes can be repainted.".into());
    }
    let pattern = format!(r"<{prefix}spPr\b[^>]*>[\s\S]*?</{prefix}spPr>");
    let (a, b) = first_span(xml, &pattern)?
        .ok_or("Drawing shape has no spPr — repainting it is not supported.")?;
    let block = &xml[a..b];
    let open_end = block
        .find('>')
        .ok_or("Drawing shape has no spPr — repainting it is not supported.")?
        + 1;
    let close_start = block.rfind("</").unwrap_or(block.len());
    let open = &block[..open_end];
    let close = &block[close_start..];
    let inner = &block[open_end..close_start];
    let old_line = first_span(inner, LINE)?
        .map(|(a, b)| inner[a..b].to_owned())
        .unwrap_or_default();
    let no_line = if let Some((a, b)) = first_span(inner, LINE)? {
        substitute(inner, a, b, "")
    } else {
        inner.to_owned()
    };
    let re = TryJsRegex::new(FILL, "g").map_err(|e| e.to_string())?;
    let mut old_fill = String::new();
    for m in re.try_find_iter(&no_line) {
        old_fill.push_str(m.map_err(|e| e.to_string())?.as_str());
    }
    let body = remove_all(&no_line, FILL)?;
    let fill = fill.map(solid).unwrap_or(old_fill);
    let line = if let Some(color) = line {
        if old_line.is_empty() {
            format!("<a:ln w=\"9525\">{}</a:ln>", solid(color))
        } else {
            let open_end = old_line.find('>').unwrap_or(old_line.len());
            let attrs = old_line["<a:ln".len()..open_end].trim_end_matches('/');
            let body = if old_line.ends_with("/>") {
                ""
            } else {
                &old_line[open_end + 1..old_line.len() - "</a:ln>".len()]
            };
            format!(
                "<a:ln{attrs}>{}{}</a:ln>",
                solid(color),
                remove_all(body, FILL)?
            )
        }
    } else {
        old_line
    };
    let geom = first_span(&body, r"<a:prstGeom\b[^>]*/>|</a:prstGeom>|</a:custGeom>")?
        .or(first_span(&body, r"</a:xfrm>")?);
    let at = geom.map_or(0, |m| m.1);
    Ok(substitute(
        xml,
        a,
        b,
        &format!("{open}{}{fill}{line}{}{close}", &body[..at], &body[at..]),
    ))
}

fn apply_one(
    xml: &str,
    prefix: &str,
    kind: &str,
    edit: &Value,
    link: Option<&str>,
    dropped: &mut Vec<String>,
) -> Result<String, String> {
    let mut out = xml.to_owned();
    let fill = edit.get("fillColor").and_then(Value::as_str);
    let line = edit.get("lineColor").and_then(Value::as_str);
    if fill.is_some() || line.is_some() {
        out = repaint_shape(&out, prefix, fill, line)?;
    }
    if let Some(anchor) = edit.get("anchor") {
        out = move_anchor(&out, prefix, kind, anchor)?;
    }
    if let Some(size) = edit.get("frameSize") {
        let (a, b) = first_span(&out, r"<a:xfrm\b[^>]*>[\s\S]*?<a:ext\b[^>]*/>")?
            .ok_or("Drawing anchor has no frame extent — resizing it is not supported.")?;
        let block = &out[a..b];
        let at = block
            .rfind("<a:ext")
            .ok_or("Drawing anchor has no frame extent — resizing it is not supported.")?;
        let width = size
            .get("width")
            .and_then(Value::as_number)
            .ok_or("Missing frame width.")?;
        let height = size
            .get("height")
            .and_then(Value::as_number)
            .ok_or("Missing frame height.")?;
        out = substitute(
            &out,
            a + at,
            b,
            &format!("<a:ext cx=\"{width}\" cy=\"{height}\"/>"),
        );
    }
    if edit.get("rotation").is_some() || edit.get("flipH").is_some() || edit.get("flipV").is_some()
    {
        let (a, b) = first_span(&out, r"<a:xfrm\b[^>]*>")?
            .ok_or("Drawing anchor has no frame transform — rotating it is not supported.")?;
        let mut tag = out[a..b].to_owned();
        if let Some(rotation) = edit.get("rotation").and_then(Value::as_f64) {
            let value = ((rotation * 60000.0 + 0.5).floor() as i64).rem_euclid(21600000);
            let value = (value != 0).then(|| value.to_string());
            tag = set_attr(&tag, "rot", value.as_deref())?;
        }
        for key in ["flipH", "flipV"] {
            if let Some(v) = edit.get(key).and_then(Value::as_bool) {
                tag = set_attr(&tag, key, v.then_some("1"))?;
            }
        }
        out = substitute(&out, a, b, &tag);
    }
    if edit.get("altText").is_some() || edit.get("hyperlink").is_some() {
        let re = TryJsRegex::new(
            r"<([A-Za-z_][\w.-]*:)?cNvPr\b[^>]*?(?:/>|>[\s\S]*?</\1cNvPr>)",
            "",
        )
        .map_err(|e| e.to_string())?;
        let found = re
            .try_find_iter(&out)
            .next()
            .transpose()
            .map_err(|e| e.to_string())?
            .ok_or("Drawing anchor has no non-visual properties element.")?;
        let block = found.as_str();
        let prefix = found.get(1).unwrap_or("");
        let self_closing = block.ends_with("/>");
        let end = if self_closing {
            block.len() - 2
        } else {
            block.find('>').unwrap_or(block.len())
        };
        let mut open = format!("{}>", &block[..end]);
        let mut inner = if self_closing {
            String::new()
        } else {
            block[end + 1..block.rfind("</").unwrap_or(block.len())].to_owned()
        };
        if let Some(text) = edit.get("altText").and_then(Value::as_str) {
            open = set_attr(&open, "descr", (!text.is_empty()).then_some(text))?;
        }
        if edit.get("hyperlink").is_some() {
            let links = TryJsRegex::new(r#"<a:hlinkClick\b[^>]*?\br:id="([^"]+)""#, "g")
                .map_err(|e| e.to_string())?;
            for m in links.try_find_iter(&inner) {
                if let Some(id) = m.map_err(|e| e.to_string())?.get(1) {
                    dropped.push(id.to_owned());
                }
            }
            inner = remove_all(
                &inner,
                r"<a:hlinkClick\b[^>]*?(?:/>|>[\s\S]*?</a:hlinkClick>)",
            )?;
            if let Some(id) = link {
                inner = format!(
                    "<a:hlinkClick xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" r:id=\"{}\"/>{inner}",
                    escape(id)
                );
            }
        }
        let rebuilt = if inner.is_empty() {
            format!("{}/>", &open[..open.len() - 1])
        } else {
            format!("{open}{inner}</{prefix}cNvPr>")
        };
        out = substitute(&out, found.start(), found.end(), &rebuilt);
    }
    if let Some(mode) = edit.get("editAs").and_then(Value::as_str) {
        if kind != "twoCellAnchor" {
            return Err("Only two-cell anchors carry a placement mode.".into());
        }
        let end = out.find('>').ok_or("Malformed drawing anchor.")? + 1;
        let tag = set_attr(&out[..end], "editAs", (mode != "twoCell").then_some(mode))?;
        out = substitute(&out, 0, end, &tag);
    }
    Ok(out)
}

#[derive(Clone)]
struct Rel {
    id: String,
    kind: String,
    target: String,
    external: bool,
    start: usize,
    end: usize,
}
fn relationships(xml: &str) -> Result<Vec<Rel>, String> {
    let rows = TryJsRegex::new(r"<Relationship\b[^>]*?(?:/>|>[\s\S]*?</Relationship>)", "g")
        .map_err(|e| e.to_string())?;
    let attr = TryJsRegex::new(r#"([A-Za-z_][\w.:-]*)\s*=\s*"([^"]*)""#, "g")
        .map_err(|e| e.to_string())?;
    let mut result = Vec::new();
    for row in rows.try_find_iter(xml) {
        let row = row.map_err(|e| e.to_string())?;
        let mut id = String::new();
        let mut kind = String::new();
        let mut target = String::new();
        let mut external = false;
        for a in attr.try_find_iter(row.as_str()) {
            let a = a.map_err(|e| e.to_string())?;
            match a.get(1).unwrap_or("") {
                "Id" => id = a.get(2).unwrap_or("").to_owned(),
                "Type" => kind = a.get(2).unwrap_or("").to_owned(),
                "Target" => {
                    target = a
                        .get(2)
                        .unwrap_or("")
                        .replace("&amp;", "&")
                        .replace("&quot;", "\"")
                        .replace("&apos;", "'")
                        .replace("&lt;", "<")
                        .replace("&gt;", ">")
                }
                "TargetMode" => external = a.get(2) == Some("External"),
                _ => {}
            }
        }
        result.push(Rel {
            id,
            kind,
            target,
            external,
            start: row.start(),
            end: row.end(),
        });
    }
    Ok(result)
}
fn owner_for_rels(path: &str) -> String {
    if path == "_rels/.rels" {
        return String::new();
    }
    match path.rsplit_once("/_rels/") {
        Some((dir, name)) => format!("{dir}/{}", name.strip_suffix(".rels").unwrap_or(name)),
        None => String::new(),
    }
}
fn has_attr_value(xml: &str, value: &str) -> Result<bool, String> {
    let re = TryJsRegex::new(r#"\s[\w:.-]+="([^"]*)""#, "g").map_err(|e| e.to_string())?;
    for found in re.try_find_iter(xml) {
        if found.map_err(|e| e.to_string())?.get(1) == Some(value) {
            return Ok(true);
        }
    }
    Ok(false)
}
fn remove_rel(xml: &str, id: &str) -> Result<String, String> {
    let rel = relationships(xml)?.into_iter().find(|r| r.id == id);
    Ok(if let Some(r) = rel {
        substitute(xml, r.start, r.end, "")
    } else {
        xml.to_owned()
    })
}
fn remove_override(xml: &str, path: &str) -> Result<String, String> {
    let re = TryJsRegex::new(r#"<Override\b[^>]*?/>"#, "g").map_err(|e| e.to_string())?;
    let target = format!("/{path}");
    let mut out = String::new();
    let mut at = 0;
    for m in re.try_find_iter(xml) {
        let m = m.map_err(|e| e.to_string())?;
        if m.as_str().contains(&format!("PartName=\"{target}\"")) {
            out.push_str(&xml[at..m.start()]);
            at = m.end();
        }
    }
    out.push_str(&xml[at..]);
    Ok(out)
}

fn collect_owned(
    pkg: &mut super::package::PackageEditor<'_>,
    roots: Vec<(String, bool)>,
    overridden_rels: &str,
    replacement: &str,
) -> Result<std::collections::HashSet<String>, String> {
    use std::collections::HashSet;
    let mut closure = HashSet::new();
    let mut queue = roots;
    while let Some((path, recursive)) = queue.pop() {
        if closure.contains(&path) || !pkg.has(&path)? {
            continue;
        }
        closure.insert(path.clone());
        let rels_path = super::pivots::rels_path_for(&path);
        if !pkg.has(&rels_path)? {
            continue;
        }
        closure.insert(rels_path.clone());
        if !recursive {
            continue;
        }
        let xml = if rels_path == overridden_rels {
            replacement.to_owned()
        } else {
            pkg.read_text(&rels_path)?.as_str().to_owned()
        };
        for rel in relationships(&xml)? {
            if rel.external {
                continue;
            }
            let valid_type = [
                "chartStyle",
                "chartColorStyle",
                "package",
                "oleObject",
                "theme",
                "themeOverride",
                "chartUserShapes",
                "image",
                "externalLink",
            ]
            .iter()
            .any(|kind| rel.kind.ends_with(&format!("/{kind}")));
            let target = super::state::resolve_rel_target(&path, &rel.target)?;
            let valid_path = [
                "xl/charts/",
                "xl/media/",
                "xl/embeddings/",
                "xl/theme/",
                "xl/drawings/",
                "xl/externalLinks/",
                "xl/oleObjects/",
            ]
            .iter()
            .any(|p| target.starts_with(p));
            if !valid_type || !valid_path {
                return Err(format!(
                    "The chart carries an unsupported relationship ({}) — deletion aborted.",
                    rel.kind
                ));
            }
            queue.push((target, true));
        }
    }
    let mut retained = HashSet::new();
    for path in pkg.paths()? {
        if !path.ends_with(".rels") || closure.contains(&path) {
            continue;
        }
        let xml = if path == overridden_rels {
            replacement.to_owned()
        } else {
            pkg.read_text(&path)?.as_str().to_owned()
        };
        let owner = owner_for_rels(&path);
        for rel in relationships(&xml)? {
            if rel.external {
                continue;
            }
            let target = super::state::resolve_rel_target(&owner, &rel.target)?;
            if closure.contains(&target) {
                retained.insert(target);
            }
        }
    }
    let mut queue: Vec<_> = retained.iter().cloned().collect();
    while let Some(path) = queue.pop() {
        let rels_path = super::pivots::rels_path_for(&path);
        if !closure.contains(&rels_path) || retained.contains(&rels_path) {
            continue;
        }
        retained.insert(rels_path.clone());
        let xml = if rels_path == overridden_rels {
            replacement.to_owned()
        } else {
            pkg.read_text(&rels_path)?.as_str().to_owned()
        };
        for rel in relationships(&xml)? {
            if rel.external {
                continue;
            }
            let target = super::state::resolve_rel_target(&path, &rel.target)?;
            if closure.contains(&target) && retained.insert(target.clone()) {
                queue.push(target);
            }
        }
    }
    Ok(closure.difference(&retained).cloned().collect())
}

fn cleanup_removed(
    pkg: &mut super::package::PackageEditor<'_>,
    drawing: &str,
    remaining: &str,
    removed: &[(String, &str)],
    touched: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    use std::collections::HashMap;
    let path = super::pivots::rels_path_for(drawing);
    if !pkg.has(&path)? {
        return Err("The drawing is missing its relationships part.".into());
    }
    let mut rels = pkg.read_text(&path)?.as_str().to_owned();
    let mut kinds = HashMap::new();
    for (id, kind) in removed {
        if kinds.get(id).is_some_and(|old| old != kind) {
            return Err("One drawing relationship is used by incompatible visuals.".into());
        }
        kinds.insert(id.clone(), *kind);
    }
    let mut roots = Vec::new();
    for (id, kind) in kinds {
        if has_attr_value(remaining, &id)? {
            continue;
        }
        let rel = relationships(&rels)?
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| format!("The deleted {kind} has no drawing relationship."))?;
        if !rel.kind.ends_with(&format!("/{kind}")) {
            return Err(format!(
                "The deleted {kind} uses an unsupported drawing relationship ({}).",
                rel.kind
            ));
        }
        rels = remove_rel(&rels, &id)?;
        if rel.external {
            continue;
        }
        let target = super::state::resolve_rel_target(drawing, &rel.target)?;
        if kind == "chart" {
            if !target.starts_with("xl/charts/")
                || !target.ends_with(".xml")
                || target["xl/charts/".len()..].contains('/')
                || !pkg.has(&target)?
            {
                return Err("The deleted chart relationship has an invalid package target.".into());
            }
            roots.push((target, true));
        } else if target.starts_with("xl/media/") && pkg.has(&target)? {
            roots.push((target, false));
        }
    }
    let removed = collect_owned(pkg, roots, &path, &rels)?;
    let mut types = if removed.is_empty() {
        String::new()
    } else {
        pkg.read_text("[Content_Types].xml")?.as_str().to_owned()
    };
    let old_types = types.clone();
    for item in &removed {
        types = remove_override(&types, item)?;
    }
    pkg.write(&path, rels)?;
    touched.insert(path);
    for item in removed {
        pkg.remove(&item);
    }
    if types != old_types {
        pkg.write("[Content_Types].xml", types)?;
        touched.insert("[Content_Types].xml".into());
    }
    Ok(())
}

fn cleanup_empty(
    pkg: &mut super::package::PackageEditor<'_>,
    drawing: &str,
    xml: &str,
    touched: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    let root = TryJsRegex::new(
        r"<(?:[A-Za-z_][\w.-]*:)?wsDr\b[^>]*>([\s\S]*)</(?:[A-Za-z_][\w.-]*:)?wsDr>",
        "",
    )
    .map_err(|e| e.to_string())?;
    let m = root
        .try_find_iter(xml)
        .next()
        .transpose()
        .map_err(|e| e.to_string())?;
    if !m
        .and_then(|m| m.get(1).map(|v| v.trim().is_empty()))
        .unwrap_or(false)
    {
        return Ok(());
    }
    let drawing_rels = super::pivots::rels_path_for(drawing);
    if pkg.has(&drawing_rels)? && !relationships(&pkg.read_text(&drawing_rels)?)?.is_empty() {
        return Ok(());
    }
    let mut incoming = Vec::new();
    for path in pkg.paths()? {
        if !path.ends_with(".rels") || path == drawing_rels {
            continue;
        }
        let owner = owner_for_rels(&path);
        for rel in relationships(&pkg.read_text(&path)?)? {
            if !rel.external && super::state::resolve_rel_target(&owner, &rel.target)? == drawing {
                incoming.push((owner.clone(), path.clone(), rel));
            }
        }
    }
    if incoming.len() != 1 {
        return Ok(());
    }
    let (owner, rels_path, rel) = incoming.pop().unwrap();
    if !owner.starts_with("xl/worksheets/")
        || !owner.ends_with(".xml")
        || owner["xl/worksheets/".len()..].contains('/')
        || !rel.kind.ends_with("/drawing")
        || rel.id.is_empty()
        || !pkg.has(&owner)?
    {
        return Ok(());
    }
    let sheet = pkg.read_text(&owner)?;
    let re = TryJsRegex::new(r"<(?:[\w.-]+:)?drawing\b[^>]*/>", "g").map_err(|e| e.to_string())?;
    let mut matches = Vec::new();
    for m in re.try_find_iter(&sheet) {
        let m = m.map_err(|e| e.to_string())?;
        if has_attr_value(m.as_str(), &rel.id)? {
            matches.push((m.start(), m.end()));
        }
    }
    if matches.len() != 1 {
        return Ok(());
    }
    let (a, b) = matches[0];
    let sheet = substitute(&sheet, a, b, "");
    if has_attr_value(&sheet, &rel.id)? {
        return Ok(());
    }
    let rels = pkg.read_text(&rels_path)?;
    let updated = remove_rel(&rels, &rel.id)?;
    if updated == *rels {
        return Ok(());
    }
    let types = pkg.read_text("[Content_Types].xml")?;
    let stripped = remove_override(&types, drawing)?;
    pkg.write(&owner, sheet)?;
    touched.insert(owner);
    if !relationships(&updated)?.is_empty() {
        pkg.write(&rels_path, updated)?;
        touched.insert(rels_path);
    } else {
        pkg.remove(&rels_path);
    }
    pkg.remove(drawing);
    if pkg.has(&drawing_rels)? {
        pkg.remove(&drawing_rels);
    }
    if stripped != *types {
        pkg.write("[Content_Types].xml", stripped)?;
        touched.insert("[Content_Types].xml".into());
    }
    Ok(())
}

fn edit_visuals(
    pkg: &mut super::package::PackageEditor<'_>,
    edits: &[Value],
    touched: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    use std::collections::HashMap;
    let mut groups: Vec<(String, Vec<&Value>)> = Vec::new();
    for edit in edits {
        let path = edit
            .get("drawingPath")
            .and_then(Value::as_str)
            .ok_or("Visual edit has no drawingPath.")?;
        if let Some((_, group)) = groups.iter_mut().find(|(p, _)| p == path) {
            group.push(edit);
        } else {
            groups.push((path.to_owned(), vec![edit]));
        }
    }
    for (path, group) in groups {
        if !pkg.has(&path)? {
            return Err(format!("Workbook is missing {path}."));
        }
        let xml = pkg.read_text(&path)?;
        let anchors = anchor_spans(&xml)?;
        let mut by_index = HashMap::new();
        for edit in &group {
            let index = edit
                .get("drawingIndex")
                .and_then(Value::as_u64)
                .ok_or("Invalid drawing index.")? as usize;
            if by_index.insert(index, *edit).is_some() {
                return Err("Duplicate edits target the same drawing anchor.".into());
            }
            if index >= anchors.len() {
                return Err(format!(
                    "Drawing anchor #{index} was not found — the file may have changed."
                ));
            }
        }
        let rels = super::pivots::rels_path_for(&path);
        let mut links = HashMap::new();
        for edit in &group {
            let index = edit.get("drawingIndex").and_then(Value::as_u64).unwrap() as usize;
            if edit.get("remove").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if let Some(url) = edit
                .get("hyperlink")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
            {
                let id = super::pivots::append_relationship(
                    pkg,
                    &rels,
                    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink",
                    url,
                    true,
                )?;
                touched.insert(rels.clone());
                links.insert(index, id);
            }
        }
        let mut removed = Vec::new();
        let mut dropped = Vec::new();
        let mut patched = Vec::with_capacity(anchors.len());
        for (index, (a, b, prefix, kind)) in anchors.iter().enumerate() {
            let anchor = &xml[*a..*b];
            let Some(edit) = by_index.get(&index) else {
                patched.push(Some(anchor.to_owned()));
                continue;
            };
            if edit.get("remove").and_then(Value::as_bool) == Some(true) {
                if anchor.contains(&format!("<{prefix}graphicFrame")) {
                    let re = TryJsRegex::new(
                        r#"<(?:[A-Za-z_][\w.-]*:)?chart\b[^>]*\br:id="([^"]+)""#,
                        "",
                    )
                    .map_err(|e| e.to_string())?;
                    let id = re
                        .try_find_iter(anchor)
                        .next()
                        .transpose()
                        .map_err(|e| e.to_string())?
                        .and_then(|m| m.get(1).map(str::to_owned))
                        .ok_or(
                            "This graphic frame is not a chart — deleting it is not supported.",
                        )?;
                    removed.push((id, "chart"));
                } else if anchor.contains(&format!("<{prefix}pic")) {
                    let re = TryJsRegex::new(r#"\s[\w.-]+:(?:embed|link)="([^"]+)""#, "g")
                        .map_err(|e| e.to_string())?;
                    for m in re.try_find_iter(anchor) {
                        if let Some(id) = m.map_err(|e| e.to_string())?.get(1) {
                            removed.push((id.to_owned(), "image"));
                        }
                    }
                }
                patched.push(None);
            } else {
                patched.push(Some(apply_one(
                    anchor,
                    prefix,
                    kind,
                    edit,
                    links.get(&index).map(String::as_str),
                    &mut dropped,
                )?));
            }
        }
        let mut movable = Vec::new();
        for (index, (a, _, _, _)) in anchors.iter().enumerate() {
            if patched[index].is_none()
                || by_index.get(&index).and_then(|e| e.get("zIndex")).is_none()
            {
                continue;
            }
            let before = &xml[..*a];
            if before.matches("<mc:AlternateContent").count()
                > before.matches("</mc:AlternateContent>").count()
            {
                continue;
            }
            movable.push(index);
        }
        let mut sorted = movable.clone();
        sorted.sort_by(|a, b| {
            let za = by_index[a]
                .get("zIndex")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let zb = by_index[b]
                .get("zIndex")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            za.total_cmp(&zb).then(a.cmp(b))
        });
        let mut placed = patched.clone();
        for (slot, from) in movable.into_iter().zip(sorted) {
            placed[slot] = patched[from].clone();
        }
        let mut next = xml.as_str().to_owned();
        for index in (0..anchors.len()).rev() {
            let (a, b, _, _) = &anchors[index];
            next.replace_range(*a..*b, placed[index].as_deref().unwrap_or(""));
        }
        if !removed.is_empty() {
            cleanup_removed(pkg, &path, &next, &removed, touched)?;
        }
        let mut orphans = Vec::new();
        for id in dropped {
            if !has_attr_value(&next, &id)? {
                orphans.push(id);
            }
        }
        if !orphans.is_empty() && pkg.has(&rels)? {
            let mut contents = pkg.read_text(&rels)?.as_str().to_owned();
            for id in orphans {
                contents = remove_rel(&contents, &id)?;
            }
            pkg.write(&rels, contents)?;
            touched.insert(rels);
        }
        pkg.write(&path, next.clone())?;
        touched.insert(path.clone());
        cleanup_empty(pkg, &path, &next, touched)?;
    }
    Ok(())
}

pub(super) fn apply(
    pkg: &mut super::package::PackageEditor<'_>,
    state: &mut super::state::PlannerState,
    resolved: &Value,
    touched: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    if let Some(edits) = resolved.get("visualEdits").and_then(Value::as_array) {
        edit_visuals(pkg, edits, touched)?;
    }
    if let Some(additions) = resolved.get("visualAdditions").and_then(Value::as_array) {
        add_visuals(pkg, state, additions, touched)?;
    }
    if let Some(additions) = resolved.get("cellImageAdditions").and_then(Value::as_array) {
        cell_images::apply(pkg, state, additions, touched)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn moves_both_markers_without_touching_picture() {
        let xml = "<xdr:twoCellAnchor><xdr:from><xdr:col>0</xdr:col></xdr:from><xdr:to><xdr:col>2</xdr:col></xdr:to><xdr:pic/></xdr:twoCellAnchor>";
        let anchor = serde_json::json!({"fromColumn": 1, "fromColumnOffset": 2, "fromRow": 3, "fromRowOffset": 4, "toColumn": 5, "toColumnOffset": 6, "toRow": 7, "toRowOffset": 8});
        let result = move_anchor(xml, "xdr:", "twoCellAnchor", &anchor).unwrap();
        assert!(result.contains("<xdr:from><xdr:col>1</xdr:col><xdr:colOff>2</xdr:colOff>"));
        assert!(result.contains("<xdr:to><xdr:col>5</xdr:col>"));
        assert!(result.contains("<xdr:pic/>"));
    }
    #[test]
    fn finds_prefixed_and_default_namespace_anchors() {
        let xml = "<wsDr><oneCellAnchor><from/></oneCellAnchor><xdr:absoluteAnchor><xdr:pic/></xdr:absoluteAnchor></wsDr>";
        let spans = anchor_spans(xml).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].2, "");
        assert_eq!(spans[1].2, "xdr:");
    }
    #[test]
    fn picture_crop_opacity_and_hyperlink_are_serialized() {
        let image = serde_json::json!({"opacity":0.5,"crop":{"left":0.1,"top":0.0,"right":0.2,"bottom":0.0},"altText":"A&B","flipH":true});
        let xml = picture_xml(7, "rId3", &image, Some("rId4"));
        assert!(xml.contains("r:embed=\"rId3\""));
        assert!(xml.contains("<a:alphaModFix amt=\"50000\"/>"));
        assert!(xml.contains("<a:srcRect l=\"10000\" r=\"20000\"/>"));
        assert!(xml.contains("descr=\"A&amp;B\""));
        assert!(xml.contains("r:id=\"rId4\""));
        assert!(xml.contains("flipH=\"1\""));
    }
    #[test]
    fn editing_transform_keeps_other_anchor_content() {
        let anchor = "<xdr:twoCellAnchor><xdr:sp><xdr:nvSpPr><xdr:cNvPr id=\"2\" name=\"Box\"/></xdr:nvSpPr><xdr:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"4\" cy=\"5\"/></a:xfrm><a:prstGeom prst=\"rect\"/><a:solidFill><a:srgbClr val=\"FFFFFF\"/></a:solidFill></xdr:spPr></xdr:sp></xdr:twoCellAnchor>";
        let edit = serde_json::json!({"rotation":90,"flipH":true,"altText":"title","fillColor":"#123456","editAs":"oneCell"});
        let result = apply_one(
            anchor,
            "xdr:",
            "twoCellAnchor",
            &edit,
            None,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(result.contains("editAs=\"oneCell\""));
        assert!(result.contains("rot=\"5400000\""));
        assert!(result.contains("flipH=\"1\""));
        assert!(result.contains("descr=\"title\""));
        assert!(result.contains("val=\"123456\""));
        assert!(result.contains("<a:ext cx=\"4\" cy=\"5\"/>"));
    }
    struct Source(std::collections::HashMap<String, String>);
    impl super::super::package::EntrySource for Source {
        fn paths(&mut self) -> Result<Vec<String>, String> {
            Ok(self.0.keys().cloned().collect())
        }
        fn read_text(&mut self, path: &str) -> Result<std::sync::Arc<String>, String> {
            self.0
                .get(path)
                .cloned()
                .map(std::sync::Arc::new)
                .ok_or_else(|| format!("Workbook is missing {path}."))
        }
        fn can_patch(&mut self, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn contains_text(&mut self, path: &str, needle: &str) -> Result<bool, String> {
            Ok(self.0.get(path).is_some_and(|v| v.contains(needle)))
        }
        fn short_date_pattern(&mut self) -> Result<String, String> {
            Ok("m/d/yyyy".into())
        }
        fn release_text(&mut self, _: &str) {}
    }
    #[test]
    fn removing_one_shared_picture_retains_media_relationship() {
        let anchor =
            "<xdr:twoCellAnchor><xdr:pic><a:blip r:embed=\"rId1\"/></xdr:pic></xdr:twoCellAnchor>";
        let drawing = format!("<xdr:wsDr>{anchor}{anchor}</xdr:wsDr>");
        let rels = "<Relationships><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"../media/image1.png\"/></Relationships>";
        let mut source = Source(std::collections::HashMap::from([
            ("xl/drawings/drawing1.xml".into(), drawing),
            ("xl/drawings/_rels/drawing1.xml.rels".into(), rels.into()),
            ("xl/media/image1.png".into(), "image".into()),
            ("[Content_Types].xml".into(), "<Types></Types>".into()),
        ]));
        let mut pkg = super::super::package::PackageEditor::new(&mut source);
        let edit = serde_json::json!({"drawingPath":"xl/drawings/drawing1.xml","drawingIndex":0,"remove":true});
        edit_visuals(&mut pkg, &[edit], &mut std::collections::HashSet::new()).unwrap();
        assert!(pkg.has("xl/media/image1.png").unwrap());
        assert!(
            pkg.read_text("xl/drawings/_rels/drawing1.xml.rels")
                .unwrap()
                .contains("rId1")
        );
        assert_eq!(
            anchor_spans(&pkg.read_text("xl/drawings/drawing1.xml").unwrap())
                .unwrap()
                .len(),
            1
        );
    }
}
