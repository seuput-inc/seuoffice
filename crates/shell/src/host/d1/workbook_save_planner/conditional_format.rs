//! Declarative conditional formatting and x14-linked rule preservation.
use super::{
    state::PlannerState,
    styles::{self, StylesheetEditor},
    validation::{area, area_ref, column, escape_text, find, js_string},
    xml,
};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

const CF_BLOCK: &str = r"<conditionalFormatting\b[^>]*?/>|<conditionalFormatting\b[^>]*>[\s\S]*?</conditionalFormatting>";
const CF_RULE: &str =
    r"<(?:\w+:)?cfRule\b[^>]*?/>|<(?:\w+:)?cfRule\b[^>]*>[\s\S]*?</(?:\w+:)?cfRule>";
const URI: &str = "{78C0D931-6437-407d-A8EE-F0AAD7539E65}";
const ICONS: &[&str] = &[
    "3Arrows",
    "3ArrowsGray",
    "3Flags",
    "3TrafficLights1",
    "3TrafficLights2",
    "3Signs",
    "3Symbols",
    "3Symbols2",
    "4Arrows",
    "4ArrowsGray",
    "4RedToBlack",
    "4Rating",
    "4TrafficLights",
    "5Arrows",
    "5ArrowsGray",
    "5Quarters",
    "5Rating",
];
fn esc(s: &str) -> String {
    escape_text(s).replace('"', "&quot;")
}
fn shown(v: Option<&Value>) -> String {
    v.map(js_string).unwrap_or_else(|| "undefined".into())
}
fn normalized(s: &str) -> String {
    let mut refs = s
        .split_whitespace()
        .map(|p| {
            let p = p.replace('$', "");
            match p.split_once(':') {
                Some((a, b)) if a == b => a.to_owned(),
                _ => p,
            }
        })
        .collect::<Vec<_>>();
    refs.sort();
    refs.join(" ")
}
fn refs(rule: &Value) -> Result<Vec<String>, String> {
    let ranges = rule
        .get("ranges")
        .and_then(Value::as_array)
        .ok_or("A conditional-formatting rule has no ranges.")?;
    if ranges.is_empty() {
        return Err("A conditional-formatting rule has no ranges.".into());
    }
    ranges
        .iter()
        .map(|v| area_ref(v, "conditional-formatting"))
        .collect()
}
fn attrs(kind: &str, priority: usize, stop: bool, dxf: Option<usize>) -> String {
    format!(
        "type=\"{kind}\"{} priority=\"{priority}\"{}",
        dxf.map(|n| format!(" dxfId=\"{n}\"")).unwrap_or_default(),
        if stop { " stopIfTrue=\"1\"" } else { "" }
    )
}
fn numeric(v: Option<&Value>) -> Option<f64> {
    let n = match v {
        None | Some(Value::Null) => 0.0,
        Some(Value::Bool(v)) => {
            if *v {
                1.0
            } else {
                0.0
            }
        }
        Some(Value::Number(n)) => n.as_f64()?,
        Some(value) => jscompat::to_number(&js_string(value)),
    };
    n.is_finite().then_some(n)
}
fn num(n: f64) -> String {
    jscompat::number_to_string(n)
}
fn color(input: &str) -> Option<String> {
    let s = input.trim();
    let h = s.strip_prefix('#').unwrap_or(s);
    if (h.len() == 6 || h.len() == 8) && h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(format!("#{}", h[..6].to_ascii_uppercase()));
    }
    let regex =
        jscompat::TryJsRegex::new(r"^rgba?\(\s*(\d+)\s*[, ]\s*(\d+)\s*[, ]\s*(\d+)", "").ok()?;
    let capture = regex.try_exec(s).ok()??;
    let mut out = String::from("#");
    for i in 1..=3 {
        let v = capture.get(i)?.parse::<u64>().ok()?.min(255);
        out.push_str(&format!("{v:02X}"));
    }
    Some(out)
}
fn required_color(v: Option<&Value>, default: &str, kind: &str) -> Result<String, String> {
    let v = v.filter(|v| !v.is_null());
    let text = v.and_then(Value::as_str).unwrap_or(default);
    let color = if v.is_none() || v.and_then(Value::as_str).is_some() {
        color(text)
    } else {
        None
    };
    color
        .map(|c| format!("FF{}", &c[1..]))
        .ok_or_else(|| format!("The {kind} color \"{}\" cannot be saved.", shown(v)))
}
fn dxf(style: Option<&Value>) -> String {
    let null = Value::Null;
    let s = style.unwrap_or(&null);
    let mut font = String::new();
    for (key, tag) in [("bl", "b"), ("it", "i")] {
        if s.get(key).and_then(Value::as_i64) == Some(1) {
            font.push_str(&format!("<{tag}/>"));
        }
    }
    for (key, tag) in [("st", "strike"), ("ul", "u")] {
        if s.get(key).and_then(|v| v.get("s")).and_then(Value::as_i64) == Some(1) {
            font.push_str(&format!("<{tag}/>"));
        }
    }
    if let Some(c) = s
        .get("cl")
        .and_then(|v| v.get("rgb"))
        .and_then(Value::as_str)
        .and_then(color)
    {
        font.push_str(&format!("<color rgb=\"FF{}\"/>", &c[1..]));
    }
    let fill = s
        .get("bg")
        .and_then(|v| v.get("rgb"))
        .and_then(Value::as_str)
        .and_then(color)
        .map(|c| {
            format!(
                "<fill><patternFill><bgColor rgb=\"FF{}\"/></patternFill></fill>",
                &c[1..]
            )
        })
        .unwrap_or_default();
    if font.is_empty() {
        format!("<dxf>{fill}</dxf>")
    } else {
        format!("<dxf><font>{font}</font>{fill}</dxf>")
    }
}
fn cfvo(v: Option<&Value>, extra: &str) -> Result<String, String> {
    let kind = v
        .and_then(|v| v.get("type"))
        .filter(|v| !v.is_null())
        .map(js_string)
        .unwrap_or_default();
    if kind == "min" || kind == "max" {
        return Ok(format!("<cfvo type=\"{kind}\"{extra}/>"));
    }
    if !["num", "percent", "percentile", "formula"].contains(&kind.as_str()) {
        return Err(format!("Unsupported threshold type \"{kind}\"."));
    }
    let raw = v.and_then(|v| v.get("value"));
    let value = if kind == "formula" {
        raw.filter(|v| !v.is_null())
            .map(js_string)
            .unwrap_or_else(|| "0".into())
    } else {
        num(numeric(raw).ok_or_else(|| format!("A {kind} threshold needs a finite value."))?)
    };
    Ok(format!(
        "<cfvo type=\"{kind}\" val=\"{}\"{extra}/>",
        esc(&value)
    ))
}
fn element(attrs: &str, formulas: &[String]) -> String {
    if formulas.is_empty() {
        format!("<cfRule {attrs}/>")
    } else {
        format!(
            "<cfRule {attrs}>{}</cfRule>",
            formulas
                .iter()
                .map(|f| format!("<formula>{}</formula>", escape_text(f)))
                .collect::<String>()
        )
    }
}
fn highlight(
    rule: &Value,
    priority: usize,
    stop: bool,
    anchor: &str,
    styles: &mut StylesheetEditor,
) -> Result<String, String> {
    let dxf = styles.intern_dxf(&dxf(rule.get("style")))?;
    let sub = rule.get("subType").and_then(Value::as_str);
    let op = rule.get("operator").and_then(Value::as_str);
    let v = rule.get("value");
    let attr = |kind| attrs(kind, priority, stop, Some(dxf));
    match sub {
        Some("number") => {
            let op = op.ok_or("A number rule needs an operator.")?;
            let null = Value::Null;
            let values = match v {
                Some(Value::Array(a)) => a.iter().collect::<Vec<_>>(),
                Some(v) => vec![v],
                None => vec![&null],
            };
            let mut fs = Vec::new();
            for val in values {
                let number = val
                    .as_f64()
                    .filter(|n| n.is_finite())
                    .ok_or("A number rule needs finite values.")?;
                fs.push(num(number));
            }
            Ok(element(
                &format!("{} operator=\"{op}\"", attr("cellIs")),
                &fs,
            ))
        }
        Some("text") => {
            let text = v.and_then(Value::as_str).unwrap_or("");
            let quoted = format!("\"{}\"", text.replace('"', "\"\""));
            let (kind, extra, formula) = match op {
                Some("containsText") => (
                    "containsText",
                    format!(" operator=\"containsText\" text=\"{}\"", esc(text)),
                    format!("NOT(ISERROR(SEARCH({quoted},{anchor})))"),
                ),
                Some("notContainsText") => (
                    "notContainsText",
                    format!(" operator=\"notContains\" text=\"{}\"", esc(text)),
                    format!("ISERROR(SEARCH({quoted},{anchor}))"),
                ),
                Some("beginsWith") => (
                    "beginsWith",
                    format!(" operator=\"beginsWith\" text=\"{}\"", esc(text)),
                    format!("LEFT({anchor},LEN({quoted}))={quoted}"),
                ),
                Some("endsWith") => (
                    "endsWith",
                    format!(" operator=\"endsWith\" text=\"{}\"", esc(text)),
                    format!("RIGHT({anchor},LEN({quoted}))={quoted}"),
                ),
                Some("equal") => ("cellIs", " operator=\"equal\"".into(), quoted),
                Some("notEqual") => ("cellIs", " operator=\"notEqual\"".into(), quoted),
                Some("containsBlanks") => (
                    "containsBlanks",
                    String::new(),
                    format!("LEN(TRIM({anchor}))=0"),
                ),
                Some("notContainsBlanks") => (
                    "notContainsBlanks",
                    String::new(),
                    format!("LEN(TRIM({anchor}))>0"),
                ),
                Some("containsErrors") => (
                    "containsErrors",
                    String::new(),
                    format!("ISERROR({anchor})"),
                ),
                Some("notContainsErrors") => (
                    "notContainsErrors",
                    String::new(),
                    format!("NOT(ISERROR({anchor}))"),
                ),
                _ => {
                    return Err(format!(
                        "Unsupported text operator \"{}\".",
                        op.unwrap_or("undefined")
                    ));
                }
            };
            Ok(element(&format!("{}{extra}", attr(kind)), &[formula]))
        }
        Some("duplicateValues") => Ok(element(&attr("duplicateValues"), &[])),
        Some("uniqueValues") => Ok(element(&attr("uniqueValues"), &[])),
        Some("rank") => {
            let rank = v
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite())
                .ok_or("A top/bottom rule needs a rank value.")?;
            Ok(element(
                &format!(
                    "{}{}{} rank=\"{}\"",
                    attr("top10"),
                    if rule.get("isPercent") == Some(&Value::Bool(true)) {
                        " percent=\"1\""
                    } else {
                        ""
                    },
                    if rule.get("isBottom") == Some(&Value::Bool(true)) {
                        " bottom=\"1\""
                    } else {
                        ""
                    },
                    num(rank)
                ),
                &[],
            ))
        }
        Some("average") => {
            let extra = match op {
                Some("greaterThan") => "",
                Some("greaterThanOrEqual") => " equalAverage=\"1\"",
                Some("lessThan") => " aboveAverage=\"0\"",
                Some("lessThanOrEqual") => " aboveAverage=\"0\" equalAverage=\"1\"",
                _ => {
                    return Err(format!(
                        "Average rule operator \"{}\" cannot be saved.",
                        op.unwrap_or("undefined")
                    ));
                }
            };
            Ok(element(&format!("{}{extra}", attr("aboveAverage")), &[]))
        }
        Some("formula") => {
            let f = v
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or("A formula rule needs a formula.")?;
            Ok(element(
                &attr("expression"),
                &[f.strip_prefix('=').unwrap_or(f).into()],
            ))
        }
        Some("timePeriod") => Err("Date-occurring rules cannot be saved yet.".into()),
        _ => Err(format!(
            "Unsupported highlight rule \"{}\".",
            shown(rule.get("subType"))
        )),
    }
}
fn serialize_rule(
    wire: &Value,
    priority: usize,
    styles: &mut StylesheetEditor,
) -> Result<String, String> {
    let refs = refs(wire)?;
    let ranges = wire
        .get("ranges")
        .and_then(Value::as_array)
        .ok_or("A conditional-formatting rule has no ranges.")?;
    let (r, _, c, _) = area(&ranges[0], "conditional-formatting")?;
    let anchor = format!("{}{}", column(c), r + 1);
    let rule = wire
        .get("rule")
        .ok_or("Invalid conditional-formatting rule.")?;
    let stop = wire.get("stopIfTrue") == Some(&Value::Bool(true));
    let kind = rule.get("type").and_then(Value::as_str);
    let content = match kind {
        Some("highlightCell") => highlight(rule, priority, stop, &anchor, styles)?,
        Some("colorScale") => {
            let mut stops = rule
                .get("config")
                .and_then(Value::as_array)
                .filter(|s| s.len() >= 2)
                .ok_or("A color scale needs at least two stops.")?
                .iter()
                .collect::<Vec<_>>();
            stops.sort_by(|a, b| {
                numeric(a.get("index"))
                    .unwrap_or(0.0)
                    .total_cmp(&numeric(b.get("index")).unwrap_or(0.0))
            });
            let mut threshold = String::new();
            let mut colors = String::new();
            for stop in stops {
                threshold.push_str(&cfvo(stop.get("value"), "")?);
                colors.push_str(&format!(
                    "<color rgb=\"{}\"/>",
                    required_color(stop.get("color"), "#FFFFFF", "color scale")?
                ));
            }
            format!(
                "<cfRule {}><colorScale>{threshold}{colors}</colorScale></cfRule>",
                attrs("colorScale", priority, stop, None)
            )
        }
        Some("dataBar") => {
            let config = rule
                .get("config")
                .filter(|v| v.is_object())
                .ok_or("A data bar rule has no configuration.")?;
            let show = if rule.get("isShowValue") == Some(&Value::Bool(false)) {
                " showValue=\"0\""
            } else {
                ""
            };
            format!(
                "<cfRule {}><dataBar{show}>{}{}<color rgb=\"{}\"/></dataBar></cfRule>",
                attrs("dataBar", priority, stop, None),
                cfvo(config.get("min"), "")?,
                cfvo(config.get("max"), "")?,
                required_color(config.get("positiveColor"), "#638EC6", "data bar")?
            )
        }
        Some("iconSet") => {
            let config = rule
                .get("config")
                .and_then(Value::as_array)
                .filter(|a| a.len() >= 2)
                .ok_or("An icon set needs at least two thresholds.")?;
            let types = config
                .iter()
                .map(|v| shown(v.get("iconType")))
                .collect::<HashSet<_>>();
            if types.len() != 1 {
                return Err("Mixed icon sets are extended-format-only and cannot be saved.".into());
            }
            let name = types.iter().next().cloned().unwrap_or_default();
            if !ICONS.contains(&name.as_str()) {
                return Err(format!("The \"{name}\" icon set cannot be saved to xlsx."));
            }
            let list = config.iter().rev().collect::<Vec<_>>();
            let ids = list
                .iter()
                .map(|v| shown(v.get("iconId")))
                .collect::<Vec<_>>();
            let ascending = (0..list.len()).map(|i| i.to_string()).collect::<Vec<_>>();
            let descending = (0..list.len())
                .rev()
                .map(|i| i.to_string())
                .collect::<Vec<_>>();
            let (natural, reversed) = if ["4Rating", "5Rating"].contains(&name.as_str()) {
                (&ascending, &descending)
            } else {
                (&descending, &ascending)
            };
            let reverse = if &ids == reversed {
                " reverse=\"1\""
            } else if &ids == natural {
                ""
            } else {
                return Err(
                    "Custom icon orderings are extended-format-only and cannot be saved.".into(),
                );
            };
            let mut values = String::new();
            for (i, item) in list.iter().enumerate() {
                if i == 0 {
                    values.push_str("<cfvo type=\"percent\" val=\"0\"/>");
                } else {
                    values.push_str(&cfvo(
                        item.get("value"),
                        if item.get("operator").and_then(Value::as_str) == Some("greaterThan") {
                            " gte=\"0\""
                        } else {
                            ""
                        },
                    )?);
                }
            }
            let show = if rule.get("isShowValue") == Some(&Value::Bool(false)) {
                " showValue=\"0\""
            } else {
                ""
            };
            format!(
                "<cfRule {}><iconSet iconSet=\"{}\"{show}{reverse}>{values}</iconSet></cfRule>",
                attrs("iconSet", priority, stop, None),
                esc(&name)
            )
        }
        _ => {
            return Err(format!(
                "Unsupported conditional-formatting rule type \"{}\".",
                shown(rule.get("type"))
            ));
        }
    };
    Ok(format!(
        "<conditionalFormatting sqref=\"{}\">{content}</conditionalFormatting>",
        refs.join(" ")
    ))
}
fn insert_tail(xml: &str, body: &str) -> Result<String, String> {
    let at=find(xml,r"<dataValidations\b|<hyperlinks\b|<printOptions\b|<pageMargins\b|<pageSetup\b|<headerFooter\b|<rowBreaks\b|<colBreaks\b|<drawing\b|<legacyDrawing\b|<picture\b|<oleObjects\b|<tableParts\b|<extLst\b")?.first().map(|v|v.0).or_else(||xml.rfind("</worksheet>")).ok_or("Worksheet has no closing element.")?;
    Ok(format!("{}{body}{}", &xml[..at], &xml[at..]))
}
struct Preserved {
    sqref: String,
    text: String,
    matched: bool,
}
fn linked_message(s: &str) -> String {
    if s.contains("type=\"dataBar\"") {
        "This range has a data-bar extension format (x14) that cannot be modified yet"
    } else {
        "This range has extended conditional formatting (x14) that cannot be modified yet"
    }
    .into()
}
fn remove_blocks(
    mut s: String,
    pattern: &str,
    predicate: impl Fn(&str) -> bool,
) -> Result<String, String> {
    for (start, end) in find(&s, pattern)?.into_iter().rev() {
        if predicate(&s[start..end]) {
            s.replace_range(start..end, "");
        }
    }
    Ok(s)
}
fn clean_twins(xml: &str, deleted: &[Preserved]) -> Result<String, String> {
    let mut ids = HashSet::new();
    let mut refs = HashSet::new();
    for b in deleted {
        let found = find(&b.text, r"<x14:id>[\s\S]*?</x14:id>")?;
        if found.is_empty() {
            refs.insert(normalized(&b.sqref));
        } else {
            for (s, e) in found {
                ids.insert(
                    b.text[s + "<x14:id>".len()..e - "</x14:id>".len()]
                        .trim()
                        .to_owned(),
                );
            }
        }
    }
    let Some(ext) = xml::root_elements(xml, "ext")?.into_iter().find(|e| {
        e.attribute("uri")
            .is_some_and(|u| u.eq_ignore_ascii_case(URI))
    }) else {
        return Ok(xml.into());
    };
    if ext.close_start == ext.end {
        return Ok(xml.into());
    }
    let original = &xml[ext.open_end..ext.close_start];
    let mut content = original.to_owned();
    content = remove_blocks(
        content,
        r"<x14:cfRule\b[^>]*?/>|<x14:cfRule\b[^>]*>[\s\S]*?</x14:cfRule>",
        |s| styles::attr(s, "id").is_some_and(|id| ids.contains(id.trim())),
    )?;
    content = remove_blocks(
        content,
        r"<x14:conditionalFormatting\b[^>]*?/>|<x14:conditionalFormatting\b[^>]*>[\s\S]*?</x14:conditionalFormatting>",
        |s| {
            let sq = s
                .split_once("<xm:sqref>")
                .and_then(|(_, s)| s.split_once("</xm:sqref>"))
                .map(|v| v.0)
                .unwrap_or("");
            (!ids.is_empty() && !s.contains("<x14:cfRule")) || refs.contains(&normalized(sq))
        },
    )?;
    content = remove_blocks(
        content,
        r"<x14:conditionalFormattings\b[^>]*?/>|<x14:conditionalFormattings\b[^>]*>[\s\S]*?</x14:conditionalFormattings>",
        |s| {
            !s.match_indices("<x14:conditionalFormatting")
                .any(|(at, _)| {
                    s.as_bytes()
                        .get(at + "<x14:conditionalFormatting".len())
                        .is_some_and(|b| b.is_ascii_whitespace() || *b == b'/' || *b == b'>')
                })
        },
    )?;
    if content == original {
        return Ok(xml.into());
    }
    if content.contains('<') {
        return Ok(format!(
            "{}{}{}",
            &xml[..ext.open_end],
            content,
            &xml[ext.close_start..]
        ));
    }
    let lists = xml::root_elements(xml, "extLst")?;
    if let Some(list) = lists
        .iter()
        .find(|e| e.start < ext.start && e.end >= ext.end)
    {
        let inside = format!(
            "{}{}",
            &xml[list.open_end..ext.start],
            &xml[ext.end..list.close_start]
        );
        if !inside.contains('<') {
            return Ok(format!("{}{}", &xml[..list.start], &xml[list.end..]));
        }
    }
    Ok(format!("{}{}", &xml[..ext.start], &xml[ext.end..]))
}
fn apply_rules(
    xml: &str,
    rules: &[Value],
    append: bool,
    styles: &mut StylesheetEditor,
) -> Result<String, String> {
    let blocks = find(xml, CF_BLOCK)?;
    let mut preserved = Vec::new();
    let mut out = xml.to_owned();
    if !append {
        for (s, e) in blocks.iter().copied().rev() {
            let block = &xml[s..e];
            if block.contains("<extLst") {
                let sqref = styles::attr(block, "sqref").ok_or_else(|| linked_message(block))?;
                preserved.push(Preserved {
                    sqref: sqref.into(),
                    text: block.into(),
                    matched: false,
                });
            } else {
                out.replace_range(s..e, "");
            }
        }
        preserved.reverse();
    }
    let mut priority = 0usize;
    for (s, e) in find(&out, CF_RULE)? {
        if let Some(p) = styles::attr(&out[s..e], "priority").and_then(|v| v.parse::<usize>().ok())
        {
            priority = priority.max(p);
        }
    }
    let mut sections = Vec::new();
    for rule in rules {
        let sqref = refs(rule)?.join(" ");
        if let Some(link) = preserved
            .iter_mut()
            .find(|p| !p.matched && normalized(&p.sqref) == normalized(&sqref))
        {
            let rule_pos = find(&link.text, CF_RULE)?.into_iter().next();
            let n = rule_pos
                .and_then(|(s, e)| styles::attr(&link.text[s..e], "priority"))
                .and_then(|v| v.parse::<usize>().ok());
            let probe = n.and_then(|n| serialize_rule(rule, n, styles).ok());
            let mut bare = link.text.clone();
            for (s, e) in find(&bare, r"<extLst\b[\s\S]*?</extLst>")?
                .into_iter()
                .rev()
                .take(1)
            {
                bare.replace_range(s..e, "");
            }
            let normalize = |s: String| -> String {
                if let Some(v) = styles::attr(&s, "sqref") {
                    let old = format!("sqref=\"{v}\"");
                    let new = format!("sqref=\"{}\"", normalized(v));
                    s.replacen(&old, &new, 1)
                } else {
                    s
                }
            };
            if probe.map(normalize) != Some(normalize(bare)) {
                return Err(linked_message(&link.text));
            }
            link.matched = true;
            continue;
        }
        priority += 1;
        sections.push(serialize_rule(rule, priority, styles)?);
    }
    let deleted = preserved
        .iter()
        .filter(|p| !p.matched)
        .map(|p| Preserved {
            sqref: p.sqref.clone(),
            text: p.text.clone(),
            matched: false,
        })
        .collect::<Vec<_>>();
    if !deleted.is_empty() {
        for p in &deleted {
            if let Some(at) = out.find(&p.text) {
                out.replace_range(at..at + p.text.len(), "");
            }
        }
        out = clean_twins(&out, &deleted)?;
    }
    if sections.is_empty() {
        return Ok(out);
    }
    let body = sections.concat();
    if append {
        if let Some((_, end)) = find(&out, CF_BLOCK)?.into_iter().last() {
            out.insert_str(end, &body);
            return Ok(out);
        }
    } else if let Some(last) = preserved.iter().rev().find(|p| p.matched)
        && let Some(at) = out.rfind(&last.text)
    {
        out.insert_str(at + last.text.len(), &body);
        return Ok(out);
    }
    insert_tail(&out, &body)
}
pub(super) fn apply(
    state: &mut PlannerState,
    resolved: &Value,
    stylesheet: Option<&mut StylesheetEditor>,
) -> Result<(), String> {
    let Some(stylesheet) = stylesheet else {
        return Ok(());
    };
    if let Some(states) = resolved.get("cfStates").and_then(Value::as_array) {
        for item in states {
            let name = item
                .get("sheetName")
                .and_then(Value::as_str)
                .ok_or("Conditional formatting needs sheetName.")?;
            if let Some(sheet) = state.worksheet_mut(name) {
                let rules = item
                    .get("rules")
                    .and_then(Value::as_array)
                    .ok_or("Conditional-formatting rules are missing.")?;
                let next = apply_rules(
                    &sheet.xml,
                    rules,
                    item.get("append") == Some(&Value::Bool(true)),
                    stylesheet,
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
    fn styles() -> StylesheetEditor {
        StylesheetEditor::new("<styleSheet><fonts count=\"1\"><font/></fonts><fills count=\"1\"><fill/></fills><cellXfs count=\"1\"><xf/></cellXfs></styleSheet>","m/d/yy").unwrap()
    }
    #[test]
    fn writes_formula_dxf_and_correct_order() {
        let rules = json!([{"ranges":[{"startRow":1,"endRow":2,"startColumn":1,"endColumn":1}],"stopIfTrue":true,"rule":{"type":"highlightCell","subType":"text","operator":"containsText","value":"a&b","style":{"bl":1,"bg":{"rgb":"#ff0000"}}}}]);
        let mut s = styles();
        let out = apply_rules(
            "<worksheet><sheetData/><dataValidations/></worksheet>",
            rules.as_array().unwrap(),
            false,
            &mut s,
        )
        .unwrap();
        assert!(out.contains("NOT(ISERROR(SEARCH(\"a&amp;b\",B2)))"));
        assert!(out.contains("dxfId=\"0\" priority=\"1\" stopIfTrue=\"1\""));
        assert!(out.find("conditionalFormatting").unwrap() < out.find("dataValidations").unwrap());
        assert!(s.serialize().contains("<bgColor rgb=\"FFFF0000\"/>"));
    }
    #[test]
    fn rejects_unrepresentable_icon_sets() {
        let rules = json!([{"ranges":[{"startRow":0,"endRow":0,"startColumn":0,"endColumn":0}],"rule":{"type":"iconSet","config":[{"iconType":"3Stars"},{"iconType":"3Stars"}]}}]);
        assert!(
            apply_rules(
                "<worksheet><sheetData/></worksheet>",
                rules.as_array().unwrap(),
                false,
                &mut styles()
            )
            .is_err()
        );
    }
    #[test]
    fn deleting_linked_rule_removes_its_x14_twin_only() {
        let xml = format!(
            "<worksheet><sheetData/><conditionalFormatting sqref=\"A1\"><cfRule type=\"dataBar\" priority=\"1\"><extLst><ext><x14:id>{{one}}</x14:id></ext></extLst></cfRule></conditionalFormatting><extLst><ext uri=\"{URI}\"><x14:conditionalFormattings><x14:conditionalFormatting><x14:cfRule id=\"{{one}}\"/><xm:sqref>A1</xm:sqref></x14:conditionalFormatting><x14:conditionalFormatting><x14:cfRule id=\"{{two}}\"/><xm:sqref>B2</xm:sqref></x14:conditionalFormatting></x14:conditionalFormattings></ext></extLst></worksheet>"
        );
        let out = apply_rules(&xml, &[], false, &mut styles()).unwrap();
        assert!(!out.contains("id=\"{one}\""));
        assert!(!out.contains("<conditionalFormatting sqref=\"A1\""));
        assert!(out.contains("id=\"{two}\""));
        assert!(out.contains(URI));
    }
    #[test]
    fn color_scale_and_data_bar_keep_thresholds() {
        let ranges = json!([{"startRow":0,"endRow":3,"startColumn":0,"endColumn":0}]);
        let rules = json!([{"ranges":ranges,"rule":{"type":"colorScale","config":[{"index":1,"value":{"type":"max"},"color":"#ffffff"},{"index":0,"value":{"type":"min"},"color":"rgb(255,0,0)"}]}},{"ranges":[{"startRow":0,"endRow":3,"startColumn":1,"endColumn":1}],"rule":{"type":"dataBar","config":{"min":{"type":"min"},"max":{"type":"max"},"positiveColor":"#638EC6"}}}]);
        let out = apply_rules(
            "<worksheet><sheetData/></worksheet>",
            rules.as_array().unwrap(),
            false,
            &mut styles(),
        )
        .unwrap();
        assert!(out.contains("<colorScale><cfvo type=\"min\"/><cfvo type=\"max\"/><color rgb=\"FFFF0000\"/><color rgb=\"FFFFFFFF\"/></colorScale>"));
        assert!(out.contains(
            "<dataBar><cfvo type=\"min\"/><cfvo type=\"max\"/><color rgb=\"FF638EC6\"/></dataBar>"
        ));
    }
}
