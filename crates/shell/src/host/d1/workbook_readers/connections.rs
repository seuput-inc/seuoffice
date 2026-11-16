//! Workbook connection and query destinations in snapshot order.
use super::{Package, resolve, workbook_xml as xml};
use jscompat::TryJsRegex;
use serde_json::{Value, json};
use shell_ipc_d1::sheets::engine::{EngineResult, engine_error};
use std::collections::HashMap;

pub(super) struct Connection {
    pub id: String,
    pub query: Value,
}
fn matches(s: &str, p: &str) -> EngineResult<Vec<String>> {
    xml::matches(p, "g", s)
}
fn first(s: &str, p: &str) -> EngineResult<Option<String>> {
    xml::first(p, "", s)
}
fn attr(s: &str, n: &str) -> EngineResult<Option<String>> {
    xml::decoded_attribute(s, n)
}
fn clip(s: &str, max: usize) -> String {
    if s.encode_utf16().count() > max {
        format!("{}…", jscompat::slice_utf16(s, 0., Some((max - 1) as f64)))
    } else {
        s.into()
    }
}
fn power_name(s: &str) -> EngineResult<Option<String>> {
    let r = TryJsRegex::new(r#"(?:^|;)\s*Location=(?:"([^"]*)"|([^;]*))"#, "i")
        .map_err(|e| engine_error(e.to_string()))?;
    Ok(r.try_exec(s)
        .map_err(|e| engine_error(e.to_string()))?
        .and_then(|m| m.get(1).or_else(|| m.get(2)))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned))
}
pub(super) fn parse(source: &str) -> EngineResult<Vec<Connection>> {
    let mut out = Vec::new();
    for xml in matches(source, r"<connection\b[^>]*?(?:/>|>[\s\S]*?</connection>)")? {
        let open = first(&xml, r"<connection\b[^>]*>")?.unwrap_or_else(|| xml.clone());
        let Some(id) = attr(&open, "id")? else {
            continue;
        };
        let name = clip(
            &attr(&open, "name")?.unwrap_or_else(|| format!("Connection {id}")),
            1000,
        );
        let desc = clip(&attr(&open, "description")?.unwrap_or_default(), 4000);
        let db = first(&xml, r"<dbPr\b[^>]*>")?;
        let connection = match &db {
            Some(d) => attr(d, "connection")?.unwrap_or_default(),
            None => String::new(),
        };
        let web = first(&xml, r"<webPr\b[^>]*>")?;
        let text = first(&xml, r"<textPr\b[^>]*>")?;
        let mut query = if connection
            .to_ascii_lowercase()
            .contains("provider=microsoft.mashup.oledb")
        {
            json!({"name":clip(&power_name(&connection)?.unwrap_or(name),1000),"kind":"powerQuery"})
        } else if let Some(web) = web {
            let mut q = json!({"name":name,"kind":"web"});
            if let Some(src) = attr(&web, "url")?.filter(|v| !v.is_empty()) {
                q["source"] = Value::String(clip(&src, 4000));
            }
            q
        } else if let Some(text) = text {
            let mut q = json!({"name":name,"kind":"text"});
            if let Some(src) = attr(&text, "sourceFile")?.filter(|v| !v.is_empty()) {
                q["source"] = Value::String(clip(&src, 4000));
            }
            q
        } else {
            json!({"name":name,"kind":if db.is_some(){"database"}else{"other"}})
        };
        if !desc.is_empty() {
            query["description"] = Value::String(desc)
        }
        out.push(Connection { id, query });
        if out.len() >= 1000 {
            break;
        }
    }
    Ok(out)
}
fn count(s: &str, name: &str, fallback: usize) -> EngineResult<usize> {
    let Some(value) = attr(s, name)? else {
        return Ok(fallback);
    };
    let n = jscompat::to_number(&value);
    Ok(
        if n.is_finite() && n >= 0. && n.fract() == 0. && n <= 9_007_199_254_740_991. {
            n as usize
        } else {
            fallback
        },
    )
}
fn valid_range(s: &str) -> EngineResult<bool> {
    TryJsRegex::new(r"^[A-Z]{1,3}\d{1,7}(?::[A-Z]{1,3}\d{1,7})?$", "i")
        .map_err(|e| engine_error(e.to_string()))?
        .try_test(s)
        .map_err(|e| engine_error(e.to_string()))
}
fn rows(s: &str, header: usize, totals: usize) -> EngineResult<usize> {
    let re = TryJsRegex::new(r"\d+", "g").map_err(|e| engine_error(e.to_string()))?;
    let numbers = re
        .try_find_iter(s)
        .map(|m| {
            m.map_err(|e| engine_error(e.to_string()))
                .map(|m| m.as_str().parse::<usize>().unwrap_or(0))
        })
        .collect::<EngineResult<Vec<_>>>()?;
    let a = numbers.first().copied().unwrap_or(0);
    let b = numbers.get(1).copied().unwrap_or(a);
    Ok(a.abs_diff(b)
        .saturating_add(1)
        .saturating_sub(header)
        .saturating_sub(totals))
}
fn defined_range(workbook: &str, name: &str, sheet_index: usize) -> EngineResult<Option<String>> {
    for item in matches(workbook, r"<definedName\b[^>]*>[^<]*</definedName>")? {
        if attr(&item, "name")?.as_deref() != Some(name) {
            continue;
        }
        if let Some(scope) = attr(&item, "localSheetId")?
            && scope.parse::<usize>().ok() != Some(sheet_index)
        {
            continue;
        }
        let Some((_, rest)) = item.split_once('>') else {
            continue;
        };
        let value = rest.split('<').next().unwrap_or("");
        let value = file_parse::xlsx::workbook_decode_xml_entities(value);
        let range = value.rsplit('!').next().unwrap_or("").replace('$', "");
        if valid_range(&range)? {
            return Ok(Some(range));
        }
    }
    Ok(None)
}
struct Table {
    reference: String,
    name: Option<String>,
    header: usize,
    totals: usize,
}
#[allow(clippy::too_many_arguments)]
fn add_query(
    pkg: &Package<'_>,
    workbook: &str,
    from: &str,
    target: &str,
    sheet: &str,
    sheet_index: usize,
    table: Option<&Table>,
    seen: &mut usize,
    dest: &mut HashMap<String, Value>,
) -> EngineResult<bool> {
    *seen += 1;
    if *seen > 500 {
        return Ok(false);
    }
    let Some(path) = resolve(from, target) else {
        return Ok(true);
    };
    let Some(content) = pkg.get(&path)? else {
        return Ok(true);
    };
    let Some(open) = first(&content, r"<queryTable\b[^>]*>")? else {
        return Ok(true);
    };
    let Some(id) = attr(&open, "connectionId")? else {
        return Ok(true);
    };
    if dest.contains_key(&id) {
        return Ok(true);
    }
    let range = match table {
        Some(t) => Some(t.reference.clone()),
        None => match attr(&open, "name")? {
            Some(n) => defined_range(workbook, &n, sheet_index)?,
            None => None,
        },
    };
    let Some(range) = range else { return Ok(true) };
    if !valid_range(&range)? {
        return Ok(true);
    }
    let header = match table {
        Some(t) => t.header,
        None => usize::from(count(&open, "headers", 1)? != 0),
    };
    let totals = table.map_or(0, |t| t.totals);
    let mut d = json!({"sheet":sheet,"hasTable":table.is_some(),"range":range,"headerRows":header,"totalsRows":totals,"rows":rows(&range,header,totals)?});
    if let Some(name) = table
        .and_then(|t| t.name.as_ref())
        .filter(|s| !s.is_empty())
    {
        d["table"] = Value::String(name.clone());
    }
    dest.insert(id, d);
    Ok(true)
}
fn collect(pkg: &Package<'_>) -> EngineResult<Vec<(String, Value)>> {
    let Some(content) = pkg.get("xl/connections.xml")? else {
        return Ok(Vec::new());
    };
    let connections = parse(&content)?;
    let workbook = pkg.get("xl/workbook.xml")?.unwrap_or_default();
    let workbook_rels = pkg.relations("xl/workbook.xml")?;
    let mut dest = HashMap::new();
    let mut seen = 0;
    'sheets: for (index, sheet) in file_parse::xlsx::workbook_sheet_elements(&workbook)
        .into_iter()
        .enumerate()
    {
        let get = |key: &str| {
            sheet
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        let rid = get("r:id").or_else(|| {
            sheet
                .iter()
                .find(|(k, _)| {
                    k.strip_suffix(":id").is_some_and(|p| {
                        !p.is_empty()
                            && p.chars().enumerate().all(|(i, c)| {
                                c.is_ascii_alphanumeric()
                                    || c == '_'
                                    || (i > 0 && (c == '-' || c == '.'))
                            })
                    })
                })
                .map(|(_, v)| v.as_str())
        });
        let (Some(name), Some(rid)) = (get("name"), rid) else {
            continue;
        };
        let Some(rel) = workbook_rels
            .iter()
            .find(|r| r.id.as_deref() == Some(rid) && !r.external)
        else {
            continue;
        };
        let Some(path) = resolve("xl/workbook.xml", &rel.target) else {
            continue;
        };
        for rel in pkg.relations(&path)? {
            if rel.external {
                continue;
            }
            if rel.kind.ends_with("/queryTable") {
                if !add_query(
                    pkg,
                    &workbook,
                    &path,
                    &rel.target,
                    name,
                    index,
                    None,
                    &mut seen,
                    &mut dest,
                )? {
                    break 'sheets;
                }
                continue;
            }
            if !rel.kind.ends_with("/table") {
                continue;
            }
            let Some(table_path) = resolve(&path, &rel.target) else {
                continue;
            };
            let Some(table_xml) = pkg.get(&table_path)? else {
                continue;
            };
            let open = first(&table_xml, r"<table\b[^>]*>")?.unwrap_or_default();
            let Some(reference) = attr(&open, "ref")? else {
                continue;
            };
            let table = Table {
                reference,
                name: attr(&open, "displayName")?.or(attr(&open, "name")?),
                header: count(&open, "headerRowCount", 1)?,
                totals: count(&open, "totalsRowCount", 0)?,
            };
            for rel in pkg.relations(&table_path)? {
                if rel.external || !rel.kind.ends_with("/queryTable") {
                    continue;
                }
                if !add_query(
                    pkg,
                    &workbook,
                    &table_path,
                    &rel.target,
                    name,
                    index,
                    Some(&table),
                    &mut seen,
                    &mut dest,
                )? {
                    break 'sheets;
                }
            }
        }
    }
    Ok(connections
        .into_iter()
        .map(|c| {
            let mut q = c.query;
            if let Some(d) = dest.get(&c.id) {
                q["destination"] = d.clone()
            }
            (c.id, q)
        })
        .collect())
}
pub(super) fn list(pkg: &Package<'_>) -> EngineResult<Vec<Value>> {
    Ok(collect(pkg)?.into_iter().map(|(_, v)| v).collect())
}
pub(super) fn refreshable(pkg: &Package<'_>) -> EngineResult<Vec<Value>> {
    let mut out = Vec::new();
    for (id, q) in collect(pkg)? {
        if q.get("kind").and_then(Value::as_str) != Some("web") {
            continue;
        }
        let Some(source) = q
            .get("source")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let Some(d) = q.get("destination") else {
            continue;
        };
        out.push(json!({"id":id,"name":q["name"],"kind":"web","source":source,"sheetName":d["sheet"],"range":d["range"],"headerRows":d["headerRows"],"totalsRows":d["totalsRows"],"hasTable":d["hasTable"]}));
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classify_sources_without_exposing_database_credentials() {
        let entries=parse(r#"<connections><connection id="1" name="Rates"><webPr url="https://example.com"/></connection><connection id="2" name="DB"><dbPr connection="secret"/></connection></connections>"#).unwrap();
        assert_eq!(entries[0].query["source"], "https://example.com");
        assert!(entries[1].query.get("source").is_none());
    }
    #[test]
    fn range_rows_exclude_headers_and_totals() {
        assert_eq!(rows("A3:D9", 1, 1).unwrap(), 5);
    }
    #[test]
    fn defined_range_respects_sheet_scope() {
        let xml = r#"<definedNames><definedName name="ExternalData_1" localSheetId="0">'A'!$A$1:$C$10</definedName><definedName name="ExternalData_1" localSheetId="1">'B'!$D$4:$F$9</definedName></definedNames>"#;
        assert_eq!(
            defined_range(xml, "ExternalData_1", 1).unwrap().as_deref(),
            Some("D4:F9")
        );
        assert_eq!(defined_range(xml, "ExternalData_1", 2).unwrap(), None);
    }
}
