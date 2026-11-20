//! Final-package external workbook link rewrites.
use super::{package::PackageEditor, state::PlannerState};
use jscompat::TryJsRegex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

fn re(pattern: &str) -> Result<TryJsRegex, String> {
    TryJsRegex::new(pattern, "").map_err(|e| e.to_string())
}
fn replace<F>(s: &str, pattern: &str, mut f: F) -> Result<String, String>
where
    F: FnMut(&str, &[Option<String>]) -> Result<String, String>,
{
    let regex = re(pattern)?;
    let mut out = String::new();
    let mut at = 0;
    for found in regex.try_find_iter(s) {
        let m = found.map_err(|e| e.to_string())?;
        out.push_str(&s[at..m.start()]);
        let groups = (1..8)
            .map(|i| m.get(i).map(str::to_owned))
            .collect::<Vec<_>>();
        out.push_str(&f(m.as_str(), &groups)?);
        at = m.end();
    }
    out.push_str(&s[at..]);
    Ok(out)
}
fn attr(s: &str, name: &str) -> Result<Option<String>, String> {
    let pattern = format!(r#"\b{}="([^"]*)""#, name);
    Ok(re(&pattern)?
        .try_exec(s)
        .map_err(|e| e.to_string())?
        .and_then(|m| m.get(1).map(str::to_owned)))
}
fn refs(body: &str) -> Result<HashSet<usize>, String> {
    let mut found = HashSet::new();
    for pattern in [
        r"'\[(\d+)\]((?:[^']|'')*)'",
        r#"\[(\d+)\]([^\s!(),;"'^&*<>=+\-*/\[\]]+)"#,
    ] {
        for m in re(pattern)?.try_find_iter(body) {
            let m = m.map_err(|e| e.to_string())?;
            if let Some(n) = m.get(1).and_then(|s| s.parse().ok()) {
                found.insert(n);
            }
        }
    }
    Ok(found)
}
fn broken_ref(body: &str, broken: &HashSet<usize>) -> Result<bool, String> {
    Ok(refs(body)?.iter().any(|n| broken.contains(n)))
}
fn renumber(body: &str, broken: &HashSet<usize>) -> Result<String, String> {
    let one = replace(body, r"'\[(\d+)\]((?:[^']|'')*)'", |whole, g| {
        let n = g[0].as_deref().unwrap_or("").parse::<usize>().unwrap_or(0);
        if broken.contains(&n) {
            return Ok(whole.into());
        }
        Ok(format!(
            "'[{}]{}'",
            n.saturating_sub(broken.iter().filter(|&&b| b < n).count()),
            g[1].as_deref().unwrap_or("")
        ))
    })?;
    replace(
        &one,
        r#"\[(\d+)\]([^\s!(),;"'^&*<>=+\-*/\[\]]+)"#,
        |whole, g| {
            let n = g[0].as_deref().unwrap_or("").parse::<usize>().unwrap_or(0);
            if broken.contains(&n) {
                return Ok(whole.into());
            }
            Ok(format!(
                "[{}]{}",
                n.saturating_sub(broken.iter().filter(|&&b| b < n).count()),
                g[1].as_deref().unwrap_or("")
            ))
        },
    )
}
fn literal(cell: &str) -> Result<String, String> {
    let opening = re(r"^<c\b([^>]*)>")?
        .try_exec(cell)
        .map_err(|e| e.to_string())?
        .and_then(|m| m.get(1).map(str::to_owned))
        .unwrap_or_default();
    let attrs = replace(
        &replace(&opening, r#"\s+t="[^"]*""#, |_, _| Ok(String::new()))?,
        r#"\s+cm="[^"]*""#,
        |_, _| Ok(String::new()),
    )?;
    let kind = attr(cell, "t")?.unwrap_or_default();
    let value = re(r"<v\b[^>]*>([\s\S]*?)</v>")?
        .try_exec(cell)
        .map_err(|e| e.to_string())?
        .and_then(|m| m.get(1).map(str::to_owned));
    match value {
        None => Ok(format!("<c{attrs} t=\"e\"><v>#REF!</v></c>")),
        Some(value) if ["str", "s", "inlineStr"].contains(&kind.as_str()) => Ok(format!(
            "<c{attrs} t=\"inlineStr\"><is><t xml:space=\"preserve\">{value}</t></is></c>"
        )),
        Some(value) if kind == "b" || kind == "e" => {
            Ok(format!("<c{attrs} t=\"{kind}\"><v>{value}</v></c>"))
        }
        Some(value) if value.is_empty() => Ok(format!("<c{attrs} t=\"e\"><v>#REF!</v></c>")),
        Some(value) => Ok(format!("<c{attrs}><v>{value}</v></c>")),
    }
}
const FORMULA: &str = r"<(f|formula|formula1|formula2|xm:f)\b([^>]*)>([\s\S]*?)</\1>";
fn sheet(xml: &str, broken: &HashSet<usize>) -> Result<String, String> {
    let mut shared = HashMap::new();
    for m in re(r#"<f t="shared"[^>]*\bsi="(\d+)"[^>]*>([\s\S]*?)</f>"#)?.try_find_iter(xml) {
        let m = m.map_err(|e| e.to_string())?;
        shared.insert(
            m.get(1).unwrap_or("").to_owned(),
            m.get(2).unwrap_or("").to_owned(),
        );
    }
    for m in re(r#"<f t="shared"[^>]*\bsi="(\d+)"[^>]*/>"#)?.try_find_iter(xml) {
        let m = m.map_err(|e| e.to_string())?;
        if let Some(body) = shared.get(m.get(1).unwrap_or(""))
            && broken_ref(body, broken)?
        {
            return Err("a shared formula references the link to break; re-enter that cell as a literal first (edit the formula, then break the link)".into());
        }
    }
    let transformed = replace(
        xml,
        r"<c\b[^>]*?(?:/>|>(?:(?!</c>)[\s\S])*</c>)",
        |cell, _| {
            if cell.ends_with("/>") {
                return Ok(cell.into());
            }
            let body = re(r"<f\b[^>]*>([\s\S]*?)</f>")?
                .try_exec(cell)
                .map_err(|e| e.to_string())?
                .and_then(|m| m.get(1).map(str::to_owned));
            if let Some(body) = body
                && broken_ref(&body, broken)?
            {
                return literal(cell);
            }
            Ok(cell.into())
        },
    )?;
    for m in re(FORMULA)?.try_find_iter(&transformed) {
        let m = m.map_err(|e| e.to_string())?;
        if broken_ref(m.get(3).unwrap_or(""), broken)? {
            let location = match m.get(1).unwrap_or("") {
                "formula" => "a conditional-formatting rule",
                "f" => "a cell formula",
                "xm:f" => "an extension-list (x14) rule",
                _ => "a data-validation rule",
            };
            return Err(format!(
                "the sheet has {location} that references the external link to break; remove or rewrite that reference first"
            ));
        }
    }
    let mut rest = transformed.clone();
    for (p, r) in [
        (r"<is>[\s\S]*?</is>", "<is/>"),
        (r"<v\b[^>]*>[\s\S]*?</v>", ""),
        (r"<t\b[^>]*>[\s\S]*?</t>", ""),
        (FORMULA, ""),
    ] {
        rest = replace(&rest, p, |_, _| Ok(r.into()))?;
    }
    if broken_ref(&rest, broken)? {
        return Err("the sheet references the external link to break outside cell formulas, conditional formatting and data validation; remove or rewrite that reference first".into());
    }
    replace(&transformed, FORMULA, |whole, g| {
        let body = g[2].as_deref().unwrap_or("");
        let next = renumber(body, broken)?;
        if body == next {
            Ok(whole.into())
        } else {
            let tag = g[0].as_deref().unwrap_or("");
            Ok(format!(
                "<{tag}{}>{next}</{tag}>",
                g[1].as_deref().unwrap_or("")
            ))
        }
    })
}
fn workbook(xml: &str, broken: &HashSet<usize>) -> Result<String, String> {
    replace(
        xml,
        r"<definedName\b([^>]*)>([\s\S]*?)</definedName>",
        |whole, g| {
            let body = g[1].as_deref().unwrap_or("");
            if broken_ref(body, broken)? {
                return Err("a defined name references the external link to break; rewrite or remove that name first".into());
            }
            let next = renumber(body, broken)?;
            if next == body {
                Ok(whole.into())
            } else {
                Ok(format!(
                    "<definedName{}>{next}</definedName>",
                    g[0].as_deref().unwrap_or("")
                ))
            }
        },
    )
}
fn chart(xml: &str, broken: &HashSet<usize>) -> Result<String, String> {
    replace(xml, r"<c:f>([\s\S]*?)</c:f>", |whole, g| {
        let body = g[0].as_deref().unwrap_or("");
        if broken_ref(body, broken)? {
            return Err("a chart range references the external link to break; remove the chart or rewrite its data range first".into());
        }
        let next = renumber(body, broken)?;
        if next == body {
            Ok(whole.into())
        } else {
            Ok(format!("<c:f>{next}</c:f>"))
        }
    })
}
fn rels_part(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{path}.rels"),
    }
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(super) fn apply(
    pkg: &mut PackageEditor<'_>,
    _state: &mut PlannerState,
    resolved: &Value,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    let Some(ops) = resolved.get("externalLinkOps").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let wp = "xl/workbook.xml";
    let rp = "xl/_rels/workbook.xml.rels";
    let mut w = (*pkg.read_text(wp)?).clone();
    let mut r = (*pkg.read_text(rp)?).clone();
    let mut links = Vec::new();
    for m in re(r#"<externalReference\b[^>]*\br:id="([^"]+)""#)?.try_find_iter(&w) {
        let m = m.map_err(|e| e.to_string())?;
        let id = m.get(1).unwrap_or("").to_owned();
        let mut part = None;
        for rel in re(r"<Relationship\b[^>]*/>")?.try_find_iter(&r) {
            let rel = rel.map_err(|e| e.to_string())?;
            if attr(rel.as_str(), "Id")?.as_deref() == Some(id.as_str())
                && attr(rel.as_str(), "Type")?
                    .as_deref()
                    .is_some_and(|t| t.ends_with("/externalLink"))
            {
                let target = attr(rel.as_str(), "Target")?.unwrap_or_default();
                part = Some(
                    target
                        .strip_prefix('/')
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("xl/{target}")),
                );
                break;
            }
        }
        links.push((id, part));
    }
    for item in ops
        .get("retargets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let index = item.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        let part = links
            .get(index.wrapping_sub(1))
            .and_then(|v| v.1.as_ref())
            .ok_or_else(|| format!("no external link {index} to point at a new source"))?;
        let path = rels_part(part);
        let xml = pkg.read_text(&path)?;
        let target = escape(
            item.get("target")
                .and_then(Value::as_str)
                .ok_or("external link target missing")?,
        );
        let mut changed = false;
        let next = replace(
            &xml,
            r#"<Relationship\b[^>]*\bType="[^"]*/externalLinkPath"[^>]*>"#,
            |whole, _| {
                if changed {
                    return Ok(whole.into());
                }
                changed = true;
                if attr(whole, "Target")?.is_some() {
                    replace(whole, r#"\bTarget="[^"]*""#, |_, _| {
                        Ok(format!("Target=\"{target}\""))
                    })
                } else {
                    Ok(whole
                        .strip_suffix("/>")
                        .map(|s| format!("{s} Target=\"{target}\"/>"))
                        .unwrap_or_else(|| whole.into()))
                }
            },
        )?;
        if !changed {
            return Err(
                "the external link has no source-path relationship to point elsewhere".into(),
            );
        }
        pkg.write(&path, next)?;
        touched.insert(path);
    }
    let broken: HashSet<usize> = ops
        .get("broken")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|v| v.as_u64().unwrap_or(0) as usize)
        .collect();
    if broken.is_empty() {
        return Ok(());
    }
    let mut ordered = broken.iter().copied().collect::<Vec<_>>();
    ordered.sort_unstable();
    for n in ordered {
        let (id, part) = links
            .get(n.wrapping_sub(1))
            .ok_or_else(|| format!("no external link {n} to break"))?;
        let part = part
            .as_ref()
            .ok_or_else(|| format!("no external link {n} to break"))?;
        w = replace(&w, r"<externalReference\b[^>]*/>", |whole, _| {
            if attr(whole, "r:id")?.as_deref() == Some(id.as_str()) {
                Ok(String::new())
            } else {
                Ok(whole.into())
            }
        })?;
        r = replace(&r, r"<Relationship\b[^>]*/>", |whole, _| {
            if attr(whole, "Id")?.as_deref() == Some(id.as_str()) {
                Ok(String::new())
            } else {
                Ok(whole.into())
            }
        })?;
        pkg.remove(part);
        let rel = rels_part(part);
        if pkg.has(&rel)? {
            pkg.remove(&rel);
        }
        if pkg.has("[Content_Types].xml")? {
            let ct = pkg.read_text("[Content_Types].xml")?;
            let ct = replace(&ct, r"<Override\b[^>]*/>", |whole, _| {
                if attr(whole, "PartName")?.as_deref() == Some(format!("/{part}").as_str()) {
                    Ok(String::new())
                } else {
                    Ok(whole.into())
                }
            })?;
            pkg.write("[Content_Types].xml", ct)?;
            touched.insert("[Content_Types].xml".into());
        }
    }
    if !w.contains("<externalReference ") && !w.contains("<externalReference/") {
        w = replace(
            &w,
            r"<externalReferences\b[^>]*>[\s\S]*?</externalReferences>",
            |_, _| Ok(String::new()),
        )?;
    }
    w = workbook(&w, &broken)?;
    for path in pkg.paths()? {
        let parent = path.rsplit_once('/').map(|p| p.0).unwrap_or("");
        if !path.ends_with(".xml") || (parent != "xl/worksheets" && parent != "xl/charts") {
            continue;
        }
        let xml = pkg.read_text(&path)?;
        let next = if parent == "xl/worksheets" {
            sheet(&xml, &broken)?
        } else {
            chart(&xml, &broken)?
        };
        if next != *xml {
            pkg.write(&path, next)?;
            touched.insert(path);
        }
    }
    pkg.write(wp, w)?;
    pkg.write(rp, r)?;
    touched.insert(wp.into());
    touched.insert(rp.into());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_values_become_literals_and_survivors_renumber() {
        let b = HashSet::from([1]);
        let x=sheet(r#"<worksheet><c r="A1" t="str"><f>'[1]S'!A1</f><v>hello</v></c><c r="A2"><f>[2]S!A1</f><v>4</v></c></worksheet>"#,&b).unwrap();
        assert!(x.contains("t=\"inlineStr\""));
        assert!(x.contains("[1]S!A1"));
    }
    #[test]
    fn broken_defined_name_is_rejected() {
        assert!(workbook("<definedName>[1]S!A1</definedName>", &HashSet::from([1])).is_err());
    }
}
