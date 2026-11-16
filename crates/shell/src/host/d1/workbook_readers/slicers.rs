//! Read slicer controls and their cache bindings without modifying the package.
use super::{Package, workbook_xml};
use jscompat::{TryJsRegex, to_number};
use serde_json::{Value, json};
use shell_ipc_d1::sheets::engine::{EngineResult, engine_error};
use std::collections::HashMap;

const REL: &str = "http://schemas.microsoft.com/office/2007/relationships/slicer";
const X14_URI: &str = "{725AE2AE-9491-48be-B2B4-4EB974FC3084}";

trait Reader {
    fn paths(&self) -> &[String];
    fn get(&self, path: &str) -> EngineResult<Option<String>>;
    fn relations(&self, path: &str) -> EngineResult<Vec<workbook_xml::Relationship>>;
}
impl Reader for Package<'_> {
    fn paths(&self) -> &[String] {
        &self.paths
    }
    fn get(&self, path: &str) -> EngineResult<Option<String>> {
        Package::get(self, path)
    }
    fn relations(&self, path: &str) -> EngineResult<Vec<workbook_xml::Relationship>> {
        Package::relations(self, path)
    }
}
fn regex(pattern: &str) -> EngineResult<TryJsRegex> {
    TryJsRegex::new(pattern, "").map_err(|e| engine_error(e.to_string()))
}
fn matches(pattern: &str, text: &str) -> EngineResult<Vec<String>> {
    regex(pattern)?
        .try_find_iter(text)
        .map(|m| {
            m.map(|v| v.as_str().to_owned())
                .map_err(|e| engine_error(e.to_string()))
        })
        .collect()
}
fn capture<'a>(pattern: &str, text: &'a str, index: usize) -> EngineResult<Option<&'a str>> {
    Ok(regex(pattern)?
        .try_exec(text)
        .map_err(|e| engine_error(e.to_string()))?
        .and_then(|m| m.get(index)))
}
fn attr<'a>(element: &'a str, name: &str) -> EngineResult<Option<&'a str>> {
    capture(
        &format!(r#"(?:^|\s){}="([^"]*)""#, jscompat::regex_escape(name)),
        element,
        1,
    )
}
fn decode(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}
fn number(s: &str) -> Option<f64> {
    let n = to_number(s);
    (n.is_finite() && n.fract() == 0.0).then_some(n)
}
fn key(n: f64) -> u64 {
    if n == 0.0 {
        0.0f64.to_bits()
    } else {
        n.to_bits()
    }
}
fn target(part: &str, target: &str) -> EngineResult<String> {
    super::super::workbook_save_planner::resolve_rel_target(part, target).map_err(engine_error)
}
fn required<R: Reader>(pkg: &R, path: &str) -> EngineResult<String> {
    pkg.get(path)?
        .ok_or_else(|| engine_error(format!("Workbook is missing {path}.")))
}
#[derive(Clone)]
struct Cache {
    path: String,
    name: String,
    source: String,
    id: Option<u64>,
    tabular: bool,
    selected: Option<Vec<f64>>,
}
fn cache(path: &str, xml: &str) -> EngineResult<Option<Cache>> {
    let Some(element) = capture(
        r"<slicerCacheDefinition(?:\s[^>]*)?(?:/>|[\s\S]*?</slicerCacheDefinition>)",
        xml,
        0,
    )?
    else {
        return Ok(None);
    };
    let (Some(name), Some(source)) = (attr(element, "name")?, attr(element, "sourceName")?) else {
        return Ok(None);
    };
    let tabular = regex(r"<tabular\b")?
        .try_test(element)
        .map_err(|e| engine_error(e.to_string()))?;
    let tag = capture(r"<tabular\b[^>]*>", element, 0)?.unwrap_or("");
    let id = attr(tag, "pivotCacheId")?
        .or(attr(element, "pivotCacheId")?)
        .and_then(number)
        .map(key);
    let mut selected = None;
    if let Some(items) = capture(r"<items\b[^>]*>([\s\S]*?)</items>", element, 1)? {
        let mut values = Vec::new();
        for item in matches(r"<i\b[^>]*/>", items)? {
            if let Some(x) = attr(&item, "x")?.and_then(number).filter(|x| *x >= 0.0)
                && attr(&item, "s")? == Some("1")
            {
                    values.push(x);
                }
        }
        selected = Some(values);
    }
    Ok(Some(Cache {
        path: path.into(),
        name: decode(name),
        source: decode(source),
        id,
        tabular,
        selected,
    }))
}
fn x14_id(xml: &str) -> EngineResult<Option<u64>> {
    for ext in matches(r"<ext\b[^>]*>[\s\S]*?</ext>", xml)? {
        let open = capture(r"<ext\b[^>]*>", &ext, 0)?.unwrap_or("");
        if attr(open, "uri")? != Some(X14_URI) {
            continue;
        }
        let tag = capture(r"<x14:pivotCacheDefinition\b[^>]*>", &ext, 0)?.unwrap_or("");
        return Ok(attr(tag, "pivotCacheId")?
            .and_then(number)
            .filter(|n| *n > 0.0)
            .map(key));
    }
    Ok(None)
}
fn controls(xml: &str) -> EngineResult<Vec<(String, String, String)>> {
    let pattern = regex(r"<slicer\b[^>]*?(/?)>")?;
    let mut out = Vec::new();
    let mut skip = 0;
    for found in pattern.try_find_iter(xml) {
        let found = found.map_err(|e| engine_error(e.to_string()))?;
        if found.start() < skip {
            continue;
        }
        if found.get(1) != Some("/") {
            let Some(close) = xml[found.end()..].find("</slicer>") else {
                break;
            };
            skip = found.end() + close + "</slicer>".len();
        }
        let tag = found.as_str();
        let (Some(name), Some(cache)) = (attr(tag, "name")?, attr(tag, "cache")?) else {
            continue;
        };
        out.push((
            decode(name),
            decode(cache),
            decode(attr(tag, "caption")?.unwrap_or(name)),
        ));
    }
    Ok(out)
}
fn read_from<R: Reader>(pkg: &R) -> EngineResult<Value> {
    let part_pattern = regex(r"^xl/slicers/[^/]+\.xml$")?;
    let mut parts = Vec::new();
    for path in pkg.paths() {
        if part_pattern
            .try_test(path)
            .map_err(|e| engine_error(e.to_string()))?
        {
            parts.push(path.clone());
        }
    }
    parts.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    if parts.is_empty() {
        return Ok(Value::Array(vec![]));
    }
    let workbook = required(pkg, "xl/workbook.xml")?;
    let rels = pkg.relations("xl/workbook.xml")?;
    let rel_target = |id: &str| -> EngineResult<Option<String>> {
        rels.iter()
            .find(|r| r.id.as_deref() == Some(id))
            .map(|r| target("xl/workbook.xml", &r.target))
            .transpose()
    };
    let mut by_cache = Vec::<(u64, String)>::new();
    for tag in matches(r"<pivotCache\b[^>]*>", &workbook)? {
        if let (Some(id), Some(rel)) =
            (attr(&tag, "cacheId")?.and_then(number), attr(&tag, "r:id")?)
            && let Some(path) = rel_target(rel)?
        {
                let k = key(id);
                if let Some((_, old)) = by_cache.iter_mut().find(|(existing, _)| *existing == k) {
                    *old = path
                } else {
                    by_cache.push((k, path));
                }
            }
    }
    let mut x14 = HashMap::new();
    let mut legacy = HashMap::new();
    for (id, path) in &by_cache {
        if let Some(xml) = pkg.get(path)? {
            match x14_id(&xml)? {
                Some(x) => {
                    x14.entry(x).or_insert_with(|| path.clone());
                }
                None => {
                    legacy.insert(*id, path.clone());
                }
            }
        }
    }
    let mut sheets = HashMap::new();
    for tag in matches(r"<sheet\b[^>]*>", &workbook)? {
        if let (Some(id), Some(rel)) = (attr(&tag, "sheetId")?, attr(&tag, "r:id")?)
            && let Some(path) = rel_target(rel)?
        {
                sheets.insert(path, format!("sheet-{}", decode(id)));
            }
    }
    let mut caches = HashMap::new();
    for path in pkg.paths() {
        if !regex(r"^xl/slicerCaches/[^/]+\.xml$")?
            .try_test(path)
            .map_err(|e| engine_error(e.to_string()))?
        {
            continue;
        }
        if let Some(c) = cache(path, &required(pkg, path)?)? {
            caches.insert(c.name.clone(), c);
        }
    }
    let mut hosts = HashMap::new();
    for path in pkg.paths() {
        if !regex(r"^xl/worksheets/[^/]+\.xml$")?
            .try_test(path)
            .map_err(|e| engine_error(e.to_string()))?
        {
            continue;
        }
        for rel in pkg.relations(path)? {
            if rel.kind == REL {
                hosts.insert(target(path, &rel.target)?, path.clone());
            }
        }
    }
    let mut result = Vec::new();
    for path in parts {
        let Some(host) = hosts.get(&path) else {
            continue;
        };
        for (name, cache_name, caption) in controls(&required(pkg, &path)?)? {
            let Some(c) = caches.get(&cache_name) else {
                continue;
            };
            if !c.tabular {
                continue;
            }
            let pivot = c.id.and_then(|id| x14.get(&id).or_else(|| legacy.get(&id)));
            result.push(json!({"sheetId":sheets.get(host).map(String::as_str).unwrap_or(""),"worksheetPath":host,"partPath":path,"cachePath":c.path,"name":name,"cacheName":c.name,"sourceName":c.source,"caption":caption,"pivotCacheDefinitionPath":pivot,"selectedItems":c.selected}));
        }
    }
    Ok(Value::Array(result))
}
pub(super) fn read(pkg: &Package<'_>) -> EngineResult<Value> {
    read_from(pkg)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Memory {
        paths: Vec<String>,
        parts: HashMap<String, String>,
    }
    impl Memory {
        fn new(entries: &[(&str, &str)]) -> Self {
            Self {
                paths: entries.iter().map(|(p, _)| (*p).into()).collect(),
                parts: entries
                    .iter()
                    .map(|(p, v)| ((*p).into(), (*v).into()))
                    .collect(),
            }
        }
    }
    impl Reader for Memory {
        fn paths(&self) -> &[String] {
            &self.paths
        }
        fn get(&self, path: &str) -> EngineResult<Option<String>> {
            Ok(self.parts.get(path).cloned())
        }
        fn relations(&self, path: &str) -> EngineResult<Vec<workbook_xml::Relationship>> {
            match self.parts.get(&workbook_xml::rels_path(path)) {
                Some(xml) => workbook_xml::relationships(xml),
                None => Ok(vec![]),
            }
        }
    }

    #[test]
    fn reads_controls_selected_items_and_x14_pivot_binding() {
        let pkg = Memory::new(&[
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet sheetId="7" r:id="s"/></sheets><pivotCaches><pivotCache cacheId="110" r:id="p"/></pivotCaches></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="s" Type="worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="p" Type="pivot" Target="pivotCache/pivotCacheDefinition1.xml"/></Relationships>"#,
            ),
            ("xl/worksheets/sheet1.xml", "<worksheet/>"),
            (
                "xl/worksheets/_rels/sheet1.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="http://schemas.microsoft.com/office/2007/relationships/slicer" Target="../slicers/slicer1.xml"/></Relationships>"#,
            ),
            (
                "xl/pivotCache/pivotCacheDefinition1.xml",
                r#"<pivotCacheDefinition><ext uri="{725AE2AE-9491-48be-B2B4-4EB974FC3084}"><x14:pivotCacheDefinition pivotCacheId="2124480673"/></ext></pivotCacheDefinition>"#,
            ),
            (
                "xl/slicerCaches/slicerCache1.xml",
                r#"<slicerCacheDefinition name="Cache&amp;1" sourceName="Region"><tabular pivotCacheId="2124480673"><items><i x="0" s="1"/><i x="1" s="0"/><i x="2" s="1"/></items></tabular></slicerCacheDefinition>"#,
            ),
            (
                "xl/slicers/slicer1.xml",
                r#"<slicers><slicer name="Filter" cache="Cache&amp;1" caption="North &amp; South"/></slicers>"#,
            ),
        ]);
        assert_eq!(
            read_from(&pkg).unwrap(),
            json!([{
                "sheetId":"sheet-7","worksheetPath":"xl/worksheets/sheet1.xml",
                "partPath":"xl/slicers/slicer1.xml","cachePath":"xl/slicerCaches/slicerCache1.xml",
                "name":"Filter","cacheName":"Cache&1","sourceName":"Region","caption":"North & South",
                "pivotCacheDefinitionPath":"xl/pivotCache/pivotCacheDefinition1.xml","selectedItems":[0.0,2.0]
            }])
        );
    }

    #[test]
    fn omits_olap_and_preserves_absent_selection() {
        let olap = cache(
            "cache",
            r#"<slicerCacheDefinition name="a" sourceName="b"><olap/></slicerCacheDefinition>"#,
        )
        .unwrap()
        .unwrap();
        assert!(!olap.tabular);
        let tabular = cache(
            "cache",
            r#"<slicerCacheDefinition name="a" sourceName="b"><tabular/></slicerCacheDefinition>"#,
        )
        .unwrap()
        .unwrap();
        assert!(tabular.tabular);
        assert_eq!(tabular.selected, None);
    }
}
