//! Page layout and sheet-scoped print names for workbook saves.
use super::{state::PlannerState, xml};
use jscompat::TryJsRegex;
use serde_json::Value;
use std::sync::Arc;

const AFTER_SETUP: &str = r"<headerFooter\b|<rowBreaks\b|<colBreaks\b|<customProperties\b|<cellWatches\b|<ignoredErrors\b|<smartTags\b|<drawing\b|<legacyDrawing\b|<legacyDrawingHF\b|<picture\b|<oleObjects\b|<controls\b|<webPublishItems\b|<tableParts\b|<extLst\b";
const AFTER_HEADER: &str = r"<rowBreaks\b|<colBreaks\b|<customProperties\b|<cellWatches\b|<ignoredErrors\b|<smartTags\b|<drawing\b|<legacyDrawing\b|<legacyDrawingHF\b|<picture\b|<oleObjects\b|<controls\b|<webPublishItems\b|<tableParts\b|<extLst\b";
const AFTER_ROW: &str = r"<colBreaks\b|<customProperties\b|<cellWatches\b|<ignoredErrors\b|<smartTags\b|<drawing\b|<legacyDrawing\b|<legacyDrawingHF\b|<picture\b|<oleObjects\b|<controls\b|<webPublishItems\b|<tableParts\b|<extLst\b";
const AFTER_COL: &str = r"<customProperties\b|<cellWatches\b|<ignoredErrors\b|<smartTags\b|<drawing\b|<legacyDrawing\b|<legacyDrawingHF\b|<picture\b|<oleObjects\b|<controls\b|<webPublishItems\b|<tableParts\b|<extLst\b";

fn find(text: &str, pattern: &str) -> Result<Option<(usize, usize)>, String> {
    let re = TryJsRegex::new(pattern, "").map_err(|e| e.to_string())?;
    Ok(re
        .try_exec(text)
        .map_err(|e| e.to_string())?
        .map(|m| (m.start(), m.end())))
}
fn splice(text: &str, start: usize, end: usize, value: &str) -> String {
    format!("{}{value}{}", &text[..start], &text[end..])
}
fn insert(xml: &str, element: &str, anchor: &str) -> Result<String, String> {
    let at = find(xml, anchor)?
        .map(|x| x.0)
        .or_else(|| xml.rfind("</worksheet>"))
        .ok_or("Worksheet has no closing element.")?;
    Ok(splice(xml, at, at, element))
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}
fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}
fn merge_attrs(
    element: &str,
    tag: &str,
    attrs: &[(String, Option<String>)],
) -> Result<String, String> {
    let mut out = element.to_owned();
    for (name, value) in attrs {
        let pattern = format!(r#" {name}="[^"]*""#);
        let found = find(&out, &pattern)?;
        out = match (found, value) {
            (Some((a, b)), Some(value)) => splice(&out, a, b, &format!(" {name}=\"{value}\"")),
            (Some((a, b)), None) => splice(&out, a, b, ""),
            (None, Some(value)) => {
                out.replacen(&format!("<{tag}"), &format!("<{tag} {name}=\"{value}\""), 1)
            }
            (None, None) => out,
        };
    }
    Ok(out)
}
fn merge_elements(
    xml: &str,
    tag: &str,
    attrs: &[(String, Option<String>)],
    anchor: &str,
) -> Result<String, String> {
    let re = TryJsRegex::new(&format!(r"<{tag}\b[^>]*?(/?)>"), "g").map_err(|e| e.to_string())?;
    let mut out = String::new();
    let mut last = 0;
    let mut any = false;
    for hit in re.try_find_iter(xml) {
        let hit = hit.map_err(|e| e.to_string())?;
        out.push_str(&xml[last..hit.start()]);
        out.push_str(&merge_attrs(hit.as_str(), tag, attrs)?);
        last = hit.end();
        any = true;
    }
    if any {
        out.push_str(&xml[last..]);
        return Ok(out);
    }
    let kept: String = attrs
        .iter()
        .filter_map(|(key, value)| value.as_ref().map(|v| format!(" {key}=\"{v}\"")))
        .collect();
    if kept.is_empty() {
        return Ok(xml.into());
    }
    insert(xml, &format!("<{tag}{kept}/>"), anchor)
}
fn set_view_attr(xml: &str, name: &str, value: Option<String>) -> Result<String, String> {
    let Some((a, b)) = find(xml, r"<sheetView\b[^>]*?/?>")? else {
        let Some(value) = value else {
            return Ok(xml.into());
        };
        let at = find(xml, r"<sheetFormatPr\b|<cols\b|<sheetData\b")?
            .ok_or("Worksheet has no sheetData element.")?
            .0;
        return Ok(splice(
            xml,
            at,
            at,
            &format!(
                "<sheetViews><sheetView workbookViewId=\"0\" {name}=\"{value}\"/></sheetViews>"
            ),
        ));
    };
    let updated = merge_attrs(&xml[a..b], "sheetView", &[(name.to_owned(), value)])?;
    Ok(splice(xml, a, b, &updated))
}
fn column_label(mut index: u64) -> String {
    let mut out = String::new();
    loop {
        out.insert(0, (b'A' + (index % 26) as u8) as char);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    out
}
fn freeze(xml: &str, rows: u64, cols: u64) -> Result<String, String> {
    let mut out = xml.to_owned();
    let Some((a, b)) = find(&out, r"<sheetView\b[^>]*?/?>")? else {
        if rows == 0 && cols == 0 {
            return Ok(out);
        }
        let at = find(&out, r"<sheetFormatPr\b|<cols\b|<sheetData\b")?
            .ok_or("Worksheet has no sheetData element.")?
            .0;
        out = splice(
            &out,
            at,
            at,
            "<sheetViews><sheetView workbookViewId=\"0\"/></sheetViews>",
        );
        return freeze(&out, rows, cols);
    };
    if out[a..b].ends_with("/>") {
        let replacement = format!("{}></sheetView>", &out[a..b - 2]);
        out = splice(&out, a, b, &replacement);
    }
    if let Some((a, b)) = find(&out, r"<pane\b[^>]*/?>(?:[\s\S]*?</pane>)?")? {
        out = splice(&out, a, b, "");
    }
    let re = TryJsRegex::new(r#"<selection\b[^>]*\bpane="[^"]*"[^>]*/?>"#, "g")
        .map_err(|e| e.to_string())?;
    out = re
        .try_replace_all(&out, |_| String::new())
        .map_err(|e| e.to_string())?;
    if rows == 0 && cols == 0 {
        return Ok(out);
    }
    let active = if rows > 0 && cols > 0 {
        "bottomRight"
    } else if rows > 0 {
        "bottomLeft"
    } else {
        "topRight"
    };
    let pane = format!(
        "<pane{}{} topLeftCell=\"{}{}\" activePane=\"{active}\" state=\"frozen\"/>",
        if cols > 0 {
            format!(" xSplit=\"{cols}\"")
        } else {
            String::new()
        },
        if rows > 0 {
            format!(" ySplit=\"{rows}\"")
        } else {
            String::new()
        },
        column_label(cols),
        rows + 1
    );
    let at = find(&out, r"<sheetView\b[^>]*?>")?
        .ok_or("Worksheet lost its sheetView element.")?
        .1;
    Ok(splice(&out, at, at, &pane))
}
fn fit_to_page(xml: &str, enabled: bool) -> Result<String, String> {
    if let Some((a, b)) = find(xml, r"<pageSetUpPr\b[^>]*/?>")? {
        let e = merge_attrs(
            &xml[a..b],
            "pageSetUpPr",
            &[("fitToPage".into(), enabled.then(|| "1".into()))],
        )?;
        return Ok(splice(xml, a, b, &e));
    }
    if !enabled {
        return Ok(xml.into());
    }
    if let Some((a, b)) = find(xml, r"<sheetPr\b[^>]*(/?)>")? {
        if xml[a..b].ends_with("/>") {
            return Ok(splice(
                xml,
                a,
                b,
                &format!(
                    "{}><pageSetUpPr fitToPage=\"1\"/></sheetPr>",
                    &xml[a..b - 2]
                ),
            ));
        }
        return Ok(splice(xml, b, b, "<pageSetUpPr fitToPage=\"1\"/>"));
    }
    let at = find(xml, r"<worksheet\b[^>]*>")?
        .ok_or("Worksheet has no root element.")?
        .1;
    Ok(splice(
        xml,
        at,
        at,
        "<sheetPr><pageSetUpPr fitToPage=\"1\"/></sheetPr>",
    ))
}
fn section(parts: &Value) -> String {
    let mut out = String::new();
    for (key, marker) in [("left", "&L"), ("center", "&C"), ("right", "&R")] {
        let Some(text) = parts
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        out.push_str(marker);
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '&' {
                out.push('&');
                if chars.peek() == Some(&'&') {
                    out.push('&');
                    chars.next();
                } else if !chars.peek().is_some_and(|c| {
                    c.is_ascii_digit()
                        || "LCRPNDTFAGBIUSEXYZOHK\"[".contains(c.to_ascii_uppercase())
                }) {
                    out.push('&');
                }
            } else {
                out.push(c);
            }
        }
    }
    escape(&out)
}
fn header_footer(
    xml: &str,
    header: Option<&Value>,
    footer: Option<&Value>,
) -> Result<String, String> {
    let re = TryJsRegex::new(
        r"<headerFooter\b([^>]*?)(?:/>(?:)|>([\s\S]*?)</headerFooter>)",
        "",
    )
    .map_err(|e| e.to_string())?;
    let hit = re.try_exec(xml).map_err(|e| e.to_string())?;
    let attrs = hit.as_ref().and_then(|m| m.get(1)).unwrap_or("").to_owned();
    let mut body = hit.as_ref().and_then(|m| m.get(2)).unwrap_or("").to_owned();
    for (tag, input) in [("oddHeader", header), ("oddFooter", footer)] {
        let Some(input) = input else {
            continue;
        };
        let text = if input.is_null() {
            String::new()
        } else {
            section(input)
        };
        let element = if text.is_empty() {
            String::new()
        } else {
            format!("<{tag}>{text}</{tag}>")
        };
        let pattern = format!(r"<{tag}\b[^>]*>[\s\S]*?</{tag}>|<{tag}\b[^>]*/>");
        if let Some((a, b)) = find(&body, &pattern)? {
            body = splice(&body, a, b, &element);
        } else if !element.is_empty() {
            let at = if tag == "oddFooter" {
                find(
                    &body,
                    r"<oddHeader\b[^>]*>[\s\S]*?</oddHeader>|<oddHeader\b[^>]*/>",
                )?
                .map_or(0, |x| x.1)
            } else {
                0
            };
            body = splice(&body, at, at, &element);
        }
    }
    let element = if body.trim().is_empty() {
        if attrs.trim().is_empty() {
            String::new()
        } else {
            format!("<headerFooter{attrs}/>")
        }
    } else {
        format!("<headerFooter{attrs}>{body}</headerFooter>")
    };
    if let Some(hit) = hit {
        Ok(splice(xml, hit.start(), hit.end(), &element))
    } else if element.is_empty() {
        Ok(xml.into())
    } else {
        insert(xml, &element, AFTER_HEADER)
    }
}
fn breaks(xml: &str, tag: &str, values: &Value, anchor: &str) -> Result<String, String> {
    let pattern = format!(r"<{tag}\b[^>]*(?:/>|>[\s\S]*?</{tag}>)");
    let mut out = if let Some((a, b)) = find(xml, &pattern)? {
        splice(xml, a, b, "")
    } else {
        xml.into()
    };
    let mut ids: Vec<u64> = values
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .filter(|n| *n > 0)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(out);
    }
    let max = if tag == "rowBreaks" { 16383 } else { 1048575 };
    let body: String = ids
        .iter()
        .map(|id| format!("<brk id=\"{id}\" max=\"{max}\" man=\"1\"/>"))
        .collect();
    let e = format!(
        "<{tag} count=\"{}\" manualBreakCount=\"{}\">{body}</{tag}>",
        ids.len(),
        ids.len()
    );
    out = insert(&out, &e, anchor)?;
    Ok(out)
}
fn apply_sheet(xml: &str, state: &Value) -> Result<String, String> {
    let mut out = xml.to_owned();
    if state.get("frozenRows").is_some() || state.get("frozenColumns").is_some() {
        out = freeze(
            &out,
            state.get("frozenRows").and_then(Value::as_u64).unwrap_or(0),
            state
                .get("frozenColumns")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )?;
    }
    for (field, attr, default) in [
        ("showGridlines", "showGridLines", true),
        ("showFormulas", "showFormulas", false),
    ] {
        if let Some(value) = state.get(field).and_then(Value::as_bool) {
            out = set_view_attr(
                &out,
                attr,
                if value == default {
                    None
                } else {
                    Some(if value { "1" } else { "0" }.into())
                },
            )?;
        }
    }
    if let Some(zoom) = state.get("zoomScale") {
        let value = if zoom.as_f64() == Some(100.0) {
            None
        } else {
            Some(value_text(zoom))
        };
        out = set_view_attr(&out, "zoomScale", value.clone())?;
        out = set_view_attr(&out, "zoomScaleNormal", value)?;
    }
    for (field, attr, default) in [
        ("showHeadings", "showRowColHeaders", true),
        ("rightToLeft", "rightToLeft", false),
    ] {
        if let Some(value) = state.get(field).and_then(Value::as_bool) {
            out = set_view_attr(
                &out,
                attr,
                if value == default {
                    None
                } else {
                    Some(if value { "1" } else { "0" }.into())
                },
            )?;
        }
    }
    let mut options = Vec::new();
    for (field, attr) in [
        ("printGridlines", "gridLines"),
        ("printHeadings", "headings"),
    ] {
        if let Some(v) = state.get(field).and_then(Value::as_bool) {
            options.push((attr.into(), v.then(|| "1".into())));
        }
    }
    if !options.is_empty() {
        out = merge_elements(
            &out,
            "printOptions",
            &options,
            r"<pageMargins\b|<pageSetup\b|<headerFooter\b|<rowBreaks\b|<colBreaks\b|<drawing\b|<legacyDrawing\b|<picture\b|<oleObjects\b|<tableParts\b|<extLst\b",
        )?;
    }
    if let Some(margins) = state.get("margins").and_then(Value::as_str) {
        let vals = match margins {
            "normal" => ["0.7", "0.7", "0.75", "0.75", "0.3", "0.3"],
            "wide" => ["1", "1", "1", "1", "0.5", "0.5"],
            "narrow" => ["0.25", "0.25", "0.75", "0.75", "0.3", "0.3"],
            _ => return Err("Unsupported page margins.".into()),
        };
        let attrs: Vec<_> = ["left", "right", "top", "bottom", "header", "footer"]
            .into_iter()
            .zip(vals)
            .map(|(a, v)| (a.into(), Some(v.into())))
            .collect();
        out = merge_elements(
            &out,
            "pageMargins",
            &attrs,
            r"<pageSetup\b|<headerFooter\b|<rowBreaks\b|<colBreaks\b|<drawing\b|<legacyDrawing\b|<picture\b|<oleObjects\b|<tableParts\b|<extLst\b",
        )?;
    }
    let mut setup = Vec::new();
    for (key, default) in [
        ("orientation", None),
        ("paperSize", None),
        ("scale", Some(100.0)),
        ("fitToWidth", Some(1.0)),
        ("fitToHeight", Some(1.0)),
    ] {
        if let Some(value) = state.get(key) {
            setup.push((
                key.into(),
                if default.is_some() && value.as_f64() == default {
                    None
                } else {
                    Some(value_text(value))
                },
            ));
        }
    }
    if !setup.is_empty() {
        out = merge_elements(&out, "pageSetup", &setup, AFTER_SETUP)?;
    }
    if let Some(v) = state.get("fitToPage").and_then(Value::as_bool) {
        out = fit_to_page(&out, v)?;
    }
    if state.get("header").is_some() || state.get("footer").is_some() {
        out = header_footer(&out, state.get("header"), state.get("footer"))?;
    }
    if let Some(v) = state.get("rowBreaks") {
        out = breaks(&out, "rowBreaks", v, AFTER_ROW)?;
    }
    if let Some(v) = state.get("colBreaks") {
        out = breaks(&out, "colBreaks", v, AFTER_COL)?;
    }
    Ok(out)
}
pub(super) fn worksheets(state: &mut PlannerState, resolved: &Value) -> Result<(), String> {
    for item in resolved
        .get("pageSetupStates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(sheet) = item
            .get("sheetName")
            .and_then(Value::as_str)
            .and_then(|name| state.worksheet_mut(name))
        {
            sheet.xml = Arc::new(apply_sheet(&sheet.xml, item)?);
        }
    }
    Ok(())
}

fn column_number(text: &str) -> Option<u64> {
    text.bytes().try_fold(0u64, |n, c| {
        c.is_ascii_alphabetic()
            .then_some(n * 26 + u64::from(c.to_ascii_uppercase() - b'A' + 1))
    })
}

fn parse_titles(text: &str) -> Result<(Option<String>, Option<String>), String> {
    let rows = TryJsRegex::new(r"^\$?(\d{1,7}):\$?(\d{1,7})$", "").map_err(|e| e.to_string())?;
    let cols = TryJsRegex::new(r"^\$?([A-Za-z]{1,3}):\$?([A-Za-z]{1,3})$", "")
        .map_err(|e| e.to_string())?;
    let invalid = || format!("Invalid print titles \"{text}\".");
    let (mut row, mut col) = (None, None);
    for part in text.split(',') {
        let part = part.trim_matches(jscompat::is_js_space);
        if let Some(m) = rows.try_exec(part).map_err(|e| e.to_string())? {
            let a = m.get(1).unwrap_or("");
            let b = m.get(2).unwrap_or("");
            if row.is_some() || a.parse::<u64>().ok() > b.parse::<u64>().ok() {
                return Err(invalid());
            }
            row = Some(format!("${a}:${b}"));
            continue;
        }
        if let Some(m) = cols.try_exec(part).map_err(|e| e.to_string())? {
            let a = m.get(1).unwrap_or("").to_ascii_uppercase();
            let b = m.get(2).unwrap_or("").to_ascii_uppercase();
            if col.is_some() || column_number(&a) > column_number(&b) {
                return Err(invalid());
            }
            col = Some(format!("${a}:${b}"));
            continue;
        }
        return Err(invalid());
    }
    Ok((row, col))
}

fn absolute_area(area: &str) -> Result<String, String> {
    let invalid = || format!("Invalid print area \"{area}\".");
    let re = TryJsRegex::new(r"^\$?([A-Z]+)\$?([1-9][0-9]*)$", "").map_err(|e| e.to_string())?;
    let parts: Vec<_> = area.split(':').collect();
    if parts.is_empty() || parts.len() > 2 {
        return Err(invalid());
    }
    let mut result = Vec::new();
    for part in parts {
        let m = re
            .try_exec(part)
            .map_err(|e| e.to_string())?
            .ok_or_else(invalid)?;
        let col = m.get(1).unwrap_or("");
        let row = m.get(2).unwrap_or("");
        if col.len() > 3 || row.len() > 7 {
            result.push(part.to_owned());
        } else {
            result.push(format!("${col}${row}"));
        }
    }
    Ok(result.join(":"))
}

fn scoped_name(xml_text: &str, name: &str, index: usize) -> Result<Option<xml::Element>, String> {
    Ok(xml::root_elements(xml_text, "definedName")?
        .into_iter()
        .find(|element| {
            element.attribute("name") == Some(name)
                && element.attribute("localSheetId") == Some(index.to_string().as_str())
        }))
}

fn title_reference(
    xml_text: &str,
    quoted: &str,
    index: usize,
    titles: &str,
) -> Result<String, String> {
    let (incoming_rows, incoming_cols) = parse_titles(titles)?;
    let mut old_rows = None;
    let mut old_cols = None;
    if let Some(element) = scoped_name(xml_text, "_xlnm.Print_Titles", index)? {
        let old = unescape(&xml_text[element.open_end..element.close_start]);
        for item in old.split(',') {
            let part = item.rsplit('!').next().unwrap_or("").replace('$', "");
            if let Ok((rows, cols)) = parse_titles(part.trim()) {
                if old_rows.is_none() {
                    old_rows = rows;
                }
                if old_cols.is_none() {
                    old_cols = cols;
                }
            }
        }
    }
    let rows = incoming_rows.or(old_rows);
    let cols = incoming_cols.or(old_cols);
    let spans: Vec<_> = [cols, rows]
        .into_iter()
        .flatten()
        .map(|span| format!("{quoted}!{span}"))
        .collect();
    if spans.is_empty() {
        return Err(format!("Invalid print titles \"{titles}\"."));
    }
    Ok(spans.join(","))
}

fn set_scoped_name(
    xml_text: &str,
    name: &str,
    index: usize,
    reference: Option<&str>,
) -> Result<String, String> {
    let mut out = if let Some(element) = scoped_name(xml_text, name, index)? {
        splice(xml_text, element.start, element.end, "")
    } else {
        xml_text.to_owned()
    };
    let Some(reference) = reference else {
        return Ok(out);
    };
    let element = format!(
        "<definedName name=\"{name}\" localSheetId=\"{index}\">{}</definedName>",
        escape(reference)
    );
    if let Some(section) = xml::root_elements(&out, "definedNames")?.into_iter().next() {
        if section.close_start == section.end {
            out = splice(
                &out,
                section.start,
                section.end,
                &format!("<definedNames>{element}</definedNames>"),
            );
        } else {
            out = splice(&out, section.open_end, section.open_end, &element);
        }
        return Ok(out);
    }
    let at = xml::root_elements(&out, "externalReferences")?
        .into_iter()
        .next()
        .map(|e| e.end)
        .or(xml::root_elements(&out, "functionGroups")?
            .into_iter()
            .next()
            .map(|e| e.end))
        .or(xml::root_elements(&out, "sheets")?
            .into_iter()
            .next()
            .map(|e| e.end))
        .ok_or("workbook.xml has no sheets section.")?;
    Ok(splice(
        &out,
        at,
        at,
        &format!("<definedNames>{element}</definedNames>"),
    ))
}

pub(super) fn print_names(state: &mut PlannerState, resolved: &Value) -> Result<(), String> {
    let names: Vec<_> = xml::root_elements(&state.workbook_xml, "sheet")?
        .into_iter()
        .filter_map(|element| element.attribute("name").map(str::to_owned))
        .collect();
    let renames = resolved
        .get("sheetPlan")
        .and_then(|plan| plan.get("renames"))
        .and_then(Value::as_array);
    let mut out = state.workbook_xml.to_string();
    let mut changed = false;
    for item in resolved
        .get("pageSetupStates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if item.get("printArea").is_none() && item.get("printTitles").is_none() {
            continue;
        }
        let old = item.get("sheetName").and_then(Value::as_str).unwrap_or("");
        let name = renames
            .into_iter()
            .flatten()
            .find(|rename| rename.get("sheetName").and_then(Value::as_str) == Some(old))
            .and_then(|rename| rename.get("newName"))
            .and_then(Value::as_str)
            .unwrap_or(old);
        let index = names
            .iter()
            .position(|n| n == name)
            .ok_or_else(|| format!("Sheet \"{name}\" was not found in the workbook."))?;
        let quoted = format!("'{}'", name.replace('\'', "''"));
        if let Some(area) = item.get("printArea") {
            let reference = if area.is_null() {
                None
            } else {
                Some(format!(
                    "{quoted}!{}",
                    absolute_area(area.as_str().unwrap_or(""))?
                ))
            };
            out = set_scoped_name(&out, "_xlnm.Print_Area", index, reference.as_deref())?;
        }
        if let Some(titles) = item.get("printTitles") {
            let reference = if titles.is_null() {
                None
            } else {
                Some(title_reference(
                    &out,
                    &quoted,
                    index,
                    titles.as_str().unwrap_or(""),
                )?)
            };
            out = set_scoped_name(&out, "_xlnm.Print_Titles", index, reference.as_deref())?;
        }
        changed = true;
    }
    if changed {
        if let Some(section) = xml::root_elements(&out, "definedNames")?.into_iter().next() {
            if section.close_start == section.end
                || out[section.open_end..section.close_start].trim().is_empty()
            {
                out = splice(&out, section.start, section.end, "");
            }
        }
        state.workbook_xml = Arc::new(out);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_preserves_unmodified_header_variants_and_writes_breaks() {
        let xml = "<worksheet><sheetViews><sheetView workbookViewId=\"0\"/></sheetViews><sheetData/><headerFooter differentOddEven=\"1\"><evenHeader>&amp;Cother</evenHeader></headerFooter></worksheet>";
        let state = serde_json::json!({"frozenRows":2,"frozenColumns":1,"header":{"center":"Page &P & stuff"},"rowBreaks":[9,3,9]});
        let out = apply_sheet(xml, &state).unwrap();
        assert!(out.contains("topLeftCell=\"B3\""));
        assert!(out.contains("<evenHeader>&amp;Cother</evenHeader>"));
        assert!(out.contains("&amp;CPage &amp;P &amp;&amp; stuff"));
        assert!(out.contains("<brk id=\"3\" max=\"16383\" man=\"1\"/><brk id=\"9\""));
    }
    #[test]
    fn names_keep_other_axis_and_validate_ranges() {
        let xml = "<workbook><sheets><sheet name=\"One\"/></sheets><definedNames><definedName name=\"_xlnm.Print_Titles\" localSheetId=\"0\">'One'!$1:$2</definedName></definedNames></workbook>";
        assert_eq!(
            title_reference(xml, "'One'", 0, "A:C").unwrap(),
            "'One'!$A:$C,'One'!$1:$2"
        );
        assert_eq!(absolute_area("A1:C10").unwrap(), "$A$1:$C$10");
        assert!(absolute_area("A:A").is_err());
        assert!(parse_titles("C:A").is_err());
    }
    #[test]
    fn print_names_follow_renamed_sheet_in_final_order() {
        let workbook = Arc::new("<workbook><sheets><sheet name=\"Other\"/><sheet name=\"New &amp; Name\"/></sheets><definedNames><definedName name=\"keep\">1</definedName></definedNames></workbook>".to_owned());
        let mut state = PlannerState {
            additions: Vec::new(),
            worksheets: Vec::new(),
            workbook_original: workbook.clone(),
            workbook_xml: workbook,
        };
        let request = serde_json::json!({
            "sheetPlan": {"renames":[{"sheetName":"Old","newName":"New & Name"}]},
            "pageSetupStates":[{"sheetName":"Old","printArea":"A1:B2","printTitles":"1:3"}]
        });
        print_names(&mut state, &request).unwrap();
        assert!(
            state.workbook_xml.contains(
                "name=\"_xlnm.Print_Area\" localSheetId=\"1\">'New &amp; Name'!$A$1:$B$2"
            )
        );
        assert!(
            state
                .workbook_xml
                .contains("name=\"_xlnm.Print_Titles\" localSheetId=\"1\">'New &amp; Name'!$1:$3")
        );
        assert!(state.workbook_xml.contains("name=\"keep\""));
    }
}
