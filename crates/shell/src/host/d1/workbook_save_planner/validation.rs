//! Declarative worksheet data-validation serialization, including x14 rules.
use super::{state::PlannerState, styles, xml};
use chrono::NaiveDate;
use jscompat::TryJsRegex;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::sync::Arc;

const URI: &str = "{CCE6A557-97BC-4b89-ADB6-D9C93CAAB3DF}";
const X14: &str = "http://schemas.microsoft.com/office/spreadsheetml/2009/9/main";
const XM: &str = "http://schemas.microsoft.com/office/excel/2006/main";
const LEGACY_SECTION: &str =
    r"<dataValidations\b[^>]*>[\s\S]*?</dataValidations>|<dataValidations\b[^>]*/>";
const LEGACY_ENTRY: &str =
    r"<dataValidation\b[^>]*?/>|<dataValidation\b[^>]*>[\s\S]*?</dataValidation>";
const X14_SECTION: &str =
    r"<x14:dataValidations\b[^>]*>[\s\S]*?</x14:dataValidations>|<x14:dataValidations\b[^>]*/>";
const X14_ENTRY: &str =
    r"<x14:dataValidation\b[^>]*?/>|<x14:dataValidation\b[^>]*>[\s\S]*?</x14:dataValidation>";

pub(super) fn find(xml: &str, pattern: &str) -> Result<Vec<(usize, usize)>, String> {
    let re = TryJsRegex::new(pattern, "g").map_err(|e| e.to_string())?;
    re.try_find_iter(xml)
        .map(|m| m.map(|m| (m.start(), m.end())).map_err(|e| e.to_string()))
        .collect()
}
pub(super) fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
pub(super) fn escape_attr(s: &str) -> String {
    escape_text(s).replace('"', "&quot;").replace('\n', "&#10;")
}
pub(super) fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => jscompat::number_to_string(n.as_f64().unwrap_or(0.0)),
        Value::Array(a) => a
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_string(v)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}
pub(super) fn column(mut n: i64) -> String {
    n += 1;
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push((b'A' + (n % 26) as u8) as char);
        n /= 26;
    }
    out.iter().rev().collect()
}
pub(super) fn area(v: &Value, what: &str) -> Result<(i64, i64, i64, i64), String> {
    let get = |key| {
        v.get(key)
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("A {what} range lies outside the worksheet grid."))
    };
    let (r1, r2, c1, c2) = (
        get("startRow")?,
        get("endRow")?,
        get("startColumn")?,
        get("endColumn")?,
    );
    if r1 < 0 || c1 < 0 || r2 < r1 || c2 < c1 || r2 >= 1_048_576 || c2 >= 16_384 {
        return Err(format!("A {what} range lies outside the worksheet grid."));
    }
    Ok((r1, r2, c1, c2))
}
pub(super) fn area_ref(v: &Value, what: &str) -> Result<String, String> {
    let (r1, r2, c1, c2) = area(v, what)?;
    let start = format!("{}{}", column(c1), r1 + 1);
    if r1 == r2 && c1 == c2 {
        Ok(start)
    } else {
        Ok(format!("{start}:{}{}", column(c2), r2 + 1))
    }
}
fn normalize_ref(s: &str) -> String {
    let s = s.replace('$', "");
    let Some((a, b)) = s.split_once(':') else {
        return s;
    };
    if a.chars().all(|c| c.is_ascii_uppercase()) && b.chars().all(|c| c.is_ascii_uppercase()) {
        return format!("{a}1:{b}1048576");
    }
    if a.chars().all(|c| c.is_ascii_digit()) && b.chars().all(|c| c.is_ascii_digit()) {
        return format!("A{a}:XFD{b}");
    }
    if a == b {
        return a.into();
    }
    s
}
fn ranges(v: &Value) -> Result<Vec<String>, String> {
    let r = v
        .as_array()
        .ok_or("A data-validation rule has no ranges.")?;
    if r.is_empty() {
        return Err("A data-validation rule has no ranges.".into());
    }
    r.iter().map(|v| area_ref(v, "data-validation")).collect()
}
fn formula(kind: Option<&str>, raw: Option<&Value>) -> Result<Option<String>, String> {
    let Some(raw) = raw.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let text = js_string(raw);
    if text.is_empty() {
        return Ok(None);
    }
    if kind == Some("list") {
        return Ok(Some(if let Some(s) = text.strip_prefix('=') {
            s.into()
        } else {
            format!("\"{}\"", text.replace('"', "\"\""))
        }));
    }
    if kind == Some("date")
        && let Some(v) = date_serial(&text)?
    {
        return Ok(Some(jscompat::number_to_string(v)));
    }
    if kind == Some("time")
        && let Some(v) = time_fraction(&text)
    {
        return Ok(Some(jscompat::number_to_string(v)));
    }
    Ok(Some(text.strip_prefix('=').unwrap_or(&text).into()))
}
fn time_fraction(text: &str) -> Option<f64> {
    let raw = text.trim().split(':').collect::<Vec<_>>();
    if raw
        .iter()
        .any(|s| s.is_empty() || s.len() > 2 || !s.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let parts = raw
        .iter()
        .map(|s| s.parse::<u32>())
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if !(parts.len() == 2 || parts.len() == 3)
        || parts[0] > 23
        || parts[1] > 59
        || parts.get(2).copied().unwrap_or(0) > 59
    {
        return None;
    }
    Some((parts[0] * 3600 + parts[1] * 60 + parts.get(2).copied().unwrap_or(0)) as f64 / 86400.0)
}
fn date_serial(text: &str) -> Result<Option<f64>, String> {
    let s = text.trim();
    let (date, time) = s
        .split_once(['T', ' '])
        .map_or((s, None), |(a, b)| (a, Some(b)));
    let pieces = date.split(['-', '/']).collect::<Vec<_>>();
    if pieces.len() != 3
        || pieces[0].len() != 4
        || pieces[1].len() > 2
        || pieces[2].len() > 2
        || pieces
            .iter()
            .any(|x| x.is_empty() || !x.chars().all(|c| c.is_ascii_digit()))
    {
        return Ok(None);
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        pieces[0].parse::<i32>(),
        pieces[1].parse::<u32>(),
        pieces[2].parse::<u32>(),
    ) else {
        return Ok(None);
    };
    if year < 1900 {
        return Err(format!(
            "Data-validation date \"{s}\" is before 1900, which Excel cannot store."
        ));
    }
    let Some(date) = NaiveDate::from_ymd_opt(year, month, day) else {
        return Ok(None);
    };
    let fraction = match time {
        Some(t) if t == t.trim() => match time_fraction(t) {
            Some(x) => x,
            None => return Ok(None),
        },
        Some(_) => return Ok(None),
        None => 0.0,
    };
    let origin = NaiveDate::from_ymd_opt(1899, 12, 30).ok_or("Invalid date origin.")?;
    let mut days = date.signed_duration_since(origin).num_days();
    if days < 61 {
        days -= 1;
    }
    Ok(Some(days as f64 + fraction))
}
struct Prepared {
    refs: Vec<String>,
    kind: Option<String>,
    rule: Map<String, Value>,
    f1: Option<String>,
    f2: Option<String>,
    x14: bool,
}
fn prepare(wire: &Value) -> Result<Prepared, String> {
    let refs = ranges(wire.get("ranges").unwrap_or(&Value::Null))?;
    let mut rule = wire
        .get("rule")
        .and_then(Value::as_object)
        .cloned()
        .ok_or("Invalid data-validation rule.")?;
    let mut kind = rule
        .get("type")
        .filter(|v| !v.is_null())
        .map(js_string)
        .unwrap_or_default();
    if kind == "listMultiple" {
        return Err("Multi-select list rules are Univer-only and cannot be saved to xlsx — delete the rule before saving.".into());
    }
    if kind == "checkbox" {
        let pair = |k: &str, d: &str| {
            rule.get(k)
                .filter(|v| v.as_str() != Some(""))
                .map(js_string)
                .unwrap_or_else(|| d.into())
        };
        let pair = format!("{},{}", pair("formula1", "1"), pair("formula2", "0"));
        rule.insert("formula1".into(), Value::String(pair));
        rule.remove("formula2");
        rule.remove("operator");
        kind = "list".into();
    }
    let kind = if kind == "any" || kind == "none" {
        None
    } else {
        Some(kind)
    };
    if let Some(t) = &kind
        && ![
            "whole",
            "decimal",
            "list",
            "date",
            "time",
            "textLength",
            "custom",
        ]
        .contains(&t.as_str())
    {
        return Err(format!("Unsupported data-validation type \"{t}\"."));
    }
    if let Some(op) = rule
        .get("operator")
        .map(js_string)
        .filter(|s| !s.is_empty())
        && ![
            "between",
            "notBetween",
            "equal",
            "notEqual",
            "greaterThan",
            "greaterThanOrEqual",
            "lessThan",
            "lessThanOrEqual",
        ]
        .contains(&op.as_str())
    {
        return Err(format!("Unsupported data-validation operator \"{op}\"."));
    }
    error_style(rule.get("errorStyle"))?;
    let f1 = formula(kind.as_deref(), rule.get("formula1"))?;
    let f2 = formula(kind.as_deref(), rule.get("formula2"))?;
    let qualified = |f: &Option<String>| -> Result<bool, String> {
        let Some(f) = f else { return Ok(false) };
        Ok(!find(f,r"(?:^|[^A-Za-z0-9_.])(?:'[^']+'|[A-Za-z_][A-Za-z0-9_.]*)(?::(?:'[^']+'|[A-Za-z_][A-Za-z0-9_.]*))?!")?.is_empty())
    };
    let x14 = if kind.as_deref() == Some("list") {
        rule.get("formula1")
            .map(js_string)
            .is_some_and(|s| s.starts_with('='))
            && qualified(&f1)?
    } else {
        qualified(&f1)? || qualified(&f2)?
    };
    Ok(Prepared {
        refs,
        kind,
        rule,
        f1,
        f2,
        x14,
    })
}
fn error_style(v: Option<&Value>) -> Result<Option<&'static str>, String> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let n = match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        value => Some(jscompat::to_number(&js_string(value))),
    };
    match n {
        Some(0.0) => Ok(Some("information")),
        Some(1.0) => Ok(None),
        Some(2.0) => Ok(Some("warning")),
        _ => Err(format!(
            "Unsupported data-validation error style \"{}\".",
            js_string(v)
        )),
    }
}
fn attributes(p: &Prepared) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    if let Some(t) = &p.kind {
        out.push(format!("type=\"{t}\""));
    }
    if let Some(op) = p
        .rule
        .get("operator")
        .map(js_string)
        .filter(|s| !s.is_empty() && s != "between")
        && p.kind
            .as_deref()
            .is_some_and(|t| t != "list" && t != "custom")
    {
        out.push(format!("operator=\"{op}\""));
    }
    for (key, attr) in [
        ("allowBlank", "allowBlank"),
        ("showInputMessage", "showInputMessage"),
        ("showErrorMessage", "showErrorMessage"),
    ] {
        if p.rule.get(key) == Some(&Value::Bool(true)) {
            out.push(format!("{attr}=\"1\""));
        }
    }
    if p.kind.as_deref() == Some("list") && p.rule.get("showDropDown") == Some(&Value::Bool(false))
    {
        let at = usize::from(p.rule.get("allowBlank") == Some(&Value::Bool(true)))
            + usize::from(p.kind.is_some());
        out.insert(at.min(out.len()), "showDropDown=\"1\"".into());
    }
    if let Some(style) = error_style(p.rule.get("errorStyle"))? {
        out.push(format!("errorStyle=\"{style}\""));
    }
    for key in ["errorTitle", "error", "promptTitle", "prompt"] {
        if let Some(s) = p
            .rule
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            out.push(format!("{key}=\"{}\"", escape_attr(s)));
        }
    }
    Ok(out)
}
fn entry(p: &Prepared) -> Result<String, String> {
    let attrs = attributes(p)?;
    if p.x14 {
        let mut body = String::new();
        for (name, f) in [("formula1", &p.f1), ("formula2", &p.f2)] {
            if let Some(f) = f {
                body.push_str(&format!(
                    "<x14:{name}><xm:f>{}</xm:f></x14:{name}>",
                    escape_text(f)
                ));
            }
        }
        Ok(format!(
            "<x14:dataValidation {}>{body}<xm:sqref>{}</xm:sqref></x14:dataValidation>",
            attrs.join(" "),
            p.refs.join(" ")
        ))
    } else {
        let mut attrs = attrs;
        attrs.push(format!("sqref=\"{}\"", p.refs.join(" ")));
        let mut body = String::new();
        for (name, f) in [("formula1", &p.f1), ("formula2", &p.f2)] {
            if let Some(f) = f {
                body.push_str(&format!("<{name}>{}</{name}>", escape_text(f)));
            }
        }
        Ok(if body.is_empty() {
            format!("<dataValidation {}/>", attrs.join(" "))
        } else {
            format!(
                "<dataValidation {}>{body}</dataValidation>",
                attrs.join(" ")
            )
        })
    }
}
fn replace_sqref(entry: &str, refs: &[&str], x14: bool) -> Result<String, String> {
    if x14 {
        let start = entry
            .find("<xm:sqref>")
            .ok_or("Invalid x14 validation range.")?
            + "<xm:sqref>".len();
        let end = entry[start..]
            .find("</xm:sqref>")
            .ok_or("Invalid x14 validation range.")?
            + start;
        Ok(format!(
            "{}{}{}",
            &entry[..start],
            refs.join(" "),
            &entry[end..]
        ))
    } else {
        let (start, end) = find(entry, r#"\bsqref="[^"]*""#)?
            .into_iter()
            .next()
            .ok_or("Invalid validation range.")?;
        Ok(format!(
            "{}sqref=\"{}\"{}",
            &entry[..start],
            refs.join(" "),
            &entry[end..]
        ))
    }
}
fn insert_legacy(xml: &str, body: &str) -> Result<String, String> {
    let anchor=find(xml,r"<hyperlinks\b|<printOptions\b|<pageMargins\b|<pageSetup\b|<headerFooter\b|<rowBreaks\b|<colBreaks\b|<drawing\b|<legacyDrawing\b|<picture\b|<oleObjects\b|<tableParts\b|<extLst\b")?.first().map(|p|p.0).or_else(||xml.rfind("</worksheet>")).ok_or("Worksheet has no closing element.")?;
    Ok(format!("{}{body}{}", &xml[..anchor], &xml[anchor..]))
}
fn remove_empty_extension(xml: &str, ext_start: usize, ext_end: usize) -> Result<String, String> {
    let lists = xml::root_elements(xml, "extLst")?;
    if let Some(list) = lists
        .iter()
        .find(|e| e.start < ext_start && e.end >= ext_end)
    {
        let contents = format!(
            "{}{}",
            &xml[list.open_end..ext_start],
            &xml[ext_end..list.close_start]
        );
        if !contents.contains('<') {
            return Ok(format!("{}{}", &xml[..list.start], &xml[list.end..]));
        }
    }
    Ok(format!("{}{}", &xml[..ext_start], &xml[ext_end..]))
}
fn reconcile_x14(xml: &str, entries: &[String]) -> Result<String, String> {
    let mut out = xml.to_owned();
    for (start, end) in find(xml, X14_SECTION)?.into_iter().rev() {
        let extensions = xml::root_elements(&out, "ext")?;
        let containing = extensions
            .into_iter()
            .filter(|e| e.start < start && e.end >= end)
            .next_back();
        out.replace_range(start..end, "");
        if let Some(ext) = containing {
            let end2 = ext.end - (end - start);
            let close = ext.close_start - (end - start);
            if !out[ext.open_end..close].contains('<') {
                out = remove_empty_extension(&out, ext.start, end2)?;
            }
        }
    }
    if entries.is_empty() {
        return Ok(out);
    }
    let block = format!(
        "<x14:dataValidations xmlns:x14=\"{X14}\" xmlns:xm=\"{XM}\" count=\"{}\">{}</x14:dataValidations>",
        entries.len(),
        entries.concat()
    );
    let ext = format!("<ext uri=\"{URI}\" xmlns:x14=\"{X14}\">{block}</ext>");
    if let Some(existing) = xml::root_elements(&out, "ext")?.into_iter().find(|e| {
        e.attribute("uri")
            .is_some_and(|v| v.eq_ignore_ascii_case(URI))
    }) {
        if existing.close_start == existing.end {
            out.replace_range(existing.start..existing.end, &ext);
        } else {
            out.insert_str(existing.close_start, &block);
        }
        return Ok(out);
    }
    let data_end = out.rfind("</sheetData>").unwrap_or(0);
    if let Some(list) = xml::root_elements(&out, "extLst")?
        .into_iter()
        .rev()
        .find(|e| e.start > data_end)
    {
        if list.close_start == list.end {
            out.replace_range(list.start..list.end, &format!("<extLst>{ext}</extLst>"));
        } else {
            out.insert_str(list.close_start, &ext);
        }
        return Ok(out);
    }
    let at = out
        .rfind("</worksheet>")
        .ok_or("Worksheet has no closing element.")?;
    out.insert_str(at, &format!("<extLst>{ext}</extLst>"));
    Ok(out)
}
fn apply_rules(
    xml: &str,
    rules: &[Value],
    append: bool,
    remove: &[Value],
) -> Result<String, String> {
    let prepared = rules.iter().map(prepare).collect::<Result<Vec<_>, _>>()?;
    let mut claimed = HashSet::new();
    if append {
        for p in &prepared {
            for r in &p.refs {
                claimed.insert(normalize_ref(r));
            }
        }
        for r in remove {
            claimed.insert(normalize_ref(&area_ref(r, "data-validation")?));
        }
    }
    let sections = find(xml, LEGACY_SECTION)?;
    let section = sections.first().copied();
    let mut legacy = Vec::new();
    let mut x14 = Vec::new();
    if append {
        if let Some((s, e)) = section {
            for (a, b) in find(&xml[s..e], LEGACY_ENTRY)? {
                let item = &xml[s + a..s + b];
                let refs = styles::attr(item, "sqref")
                    .unwrap_or("")
                    .split_whitespace()
                    .collect::<Vec<_>>();
                let keep = refs
                    .iter()
                    .copied()
                    .filter(|r| !claimed.contains(&normalize_ref(r)))
                    .collect::<Vec<_>>();
                if !keep.is_empty() {
                    legacy.push(if keep.len() == refs.len() {
                        item.into()
                    } else {
                        replace_sqref(item, &keep, false)?
                    });
                }
            }
        }
        for (s, e) in find(xml, X14_ENTRY)? {
            let item = &xml[s..e];
            let refs = item
                .split_once("<xm:sqref>")
                .and_then(|(_, tail)| tail.split_once("</xm:sqref>"))
                .map(|(v, _)| v)
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>();
            let keep = refs
                .iter()
                .copied()
                .filter(|r| !claimed.contains(&normalize_ref(r)))
                .collect::<Vec<_>>();
            if !keep.is_empty() {
                x14.push(if keep.len() == refs.len() {
                    item.into()
                } else {
                    replace_sqref(item, &keep, true)?
                });
            }
        }
    }
    for p in &prepared {
        if p.x14 {
            x14.push(entry(p)?)
        } else {
            legacy.push(entry(p)?);
        }
    }
    let mut out = xml.to_owned();
    if append {
        if let Some((s, e)) = section {
            out.replace_range(s..e, "");
            if !legacy.is_empty() {
                out.insert_str(
                    s,
                    &format!(
                        "<dataValidations count=\"{}\">{}</dataValidations>",
                        legacy.len(),
                        legacy.concat()
                    ),
                );
            }
        } else if !legacy.is_empty() {
            out = insert_legacy(
                &out,
                &format!(
                    "<dataValidations count=\"{}\">{}</dataValidations>",
                    legacy.len(),
                    legacy.concat()
                ),
            )?;
        }
    } else {
        for (s, e) in sections.into_iter().rev() {
            out.replace_range(s..e, "");
        }
        if !legacy.is_empty() {
            out = insert_legacy(
                &out,
                &format!(
                    "<dataValidations count=\"{}\">{}</dataValidations>",
                    legacy.len(),
                    legacy.concat()
                ),
            )?;
        }
    }
    reconcile_x14(&out, &x14)
}
pub(super) fn apply(state: &mut PlannerState, resolved: &Value) -> Result<(), String> {
    if let Some(states) = resolved.get("dvStates").and_then(Value::as_array) {
        for item in states {
            let name = item
                .get("sheetName")
                .and_then(Value::as_str)
                .ok_or("Data validation needs sheetName.")?;
            if let Some(sheet) = state.worksheet_mut(name) {
                let rules = item
                    .get("rules")
                    .and_then(Value::as_array)
                    .ok_or("Data-validation rules are missing.")?;
                let remove = item
                    .get("remove")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let next = apply_rules(
                    &sheet.xml,
                    rules,
                    item.get("append") == Some(&Value::Bool(true)),
                    remove,
                )?;
                sheet.xml = Arc::new(next);
            }
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn rewrites_and_reranges_validation() {
        let xml = "<worksheet><sheetData/><dataValidations count=\"1\"><dataValidation type=\"whole\" sqref=\"A1 B2\"/></dataValidations><pageMargins/></worksheet>";
        let r = json!([{"ranges":[{"startRow":0,"endRow":0,"startColumn":0,"endColumn":0}],"rule":{"type":"list","formula1":"one,two"}}]);
        let out = apply_rules(xml, r.as_array().unwrap(), true, &[]).unwrap();
        assert!(out.contains("sqref=\"B2\""));
        assert!(out.contains("<formula1>\"one,two\"</formula1>"));
        assert!(out.find("dataValidations").unwrap() < out.find("pageMargins").unwrap());
    }
    #[test]
    fn cross_sheet_rules_use_the_validation_extension() {
        let r = json!([{"ranges":[{"startRow":0,"endRow":0,"startColumn":0,"endColumn":0}],"rule":{"type":"list","formula1":"='Other'!A1:A3"}}]);
        let out = apply_rules(
            "<worksheet><sheetData/></worksheet>",
            r.as_array().unwrap(),
            false,
            &[],
        )
        .unwrap();
        assert!(out.contains(URI));
        assert!(out.contains("<xm:f>'Other'!A1:A3</xm:f>"));
        assert!(!out.contains("<dataValidations "));
    }
    #[test]
    fn date_and_time_values_follow_excel_serials() {
        assert_eq!(date_serial("1900-01-01").unwrap(), Some(1.0));
        assert_eq!(date_serial("1900-03-01").unwrap(), Some(61.0));
        assert_eq!(time_fraction("12:00"), Some(0.5));
        assert!(date_serial("1899-12-31").is_err());
        assert_eq!(date_serial("2024-02-30").unwrap(), None);
    }
    #[test]
    fn append_removes_only_claimed_x14_ranges() {
        let xml = format!(
            "<worksheet><sheetData/><extLst><ext uri=\"{URI}\"><x14:dataValidations><x14:dataValidation><xm:sqref>A1 B2</xm:sqref></x14:dataValidation></x14:dataValidations></ext></extLst></worksheet>"
        );
        let remove = json!([{"startRow":0,"endRow":0,"startColumn":0,"endColumn":0}]);
        let out = apply_rules(&xml, &[], true, remove.as_array().unwrap()).unwrap();
        assert!(out.contains("<xm:sqref>B2</xm:sqref>"));
        assert!(out.contains(URI));
    }
}
