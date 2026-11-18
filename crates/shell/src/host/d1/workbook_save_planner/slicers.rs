use jscompat::TryJsRegex;
use std::collections::HashSet;
use super::{package::PackageEditor, pivots::{append_relationship, relative_target, rels_path_for}};

const XML_DECL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
const X14_NS: &str = "http://schemas.microsoft.com/office/spreadsheetml/2009/9/main";
const REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

pub(super) struct SlicerItem {
    pub x: serde_json::Number,
    pub selected: bool,
}

fn escape_attribute(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        .replace('"', "&quot;").replace('\'', "&apos;")
}

fn first<'a>(xml: &'a str, pattern: &str) -> Result<Option<&'a str>, String> {
    let re = TryJsRegex::new(pattern, "").map_err(|e| e.to_string())?;
    let found = re.try_find_iter(xml).next().transpose().map_err(|e| e.to_string())?;
    Ok(found.map(|m| &xml[m.start()..m.end()]))
}

fn retained_attributes(tag: &str, skip: &[&str]) -> Result<(String, bool), String> {
    let re = TryJsRegex::new(r#"([A-Za-z_][\w.:-]*)\s*=\s*("[^"]*"|'[^']*')"#, "")
        .map_err(|e| e.to_string())?;
    let mut result = String::new();
    let mut cross_filter = false;
    for attr in re.try_find_iter(tag) {
        let attr = attr.map_err(|e| e.to_string())?;
        let Some(name) = attr.get(1) else { continue };
        if skip.contains(&name) { continue; }
        if name == "crossFilter" { cross_filter = true; }
        result.push(' ');
        result.push_str(name);
        result.push('=');
        result.push_str(attr.get(2).unwrap_or(""));
    }
    Ok((result, cross_filter))
}

fn attribute(tag: &str, name: &str) -> Result<Option<String>, String> {
    let re = TryJsRegex::new(r#"([A-Za-z_][\w.:-]*)\s*=\s*("[^"]*"|'[^']*')"#, "")
        .map_err(|e| e.to_string())?;
    for attr in re.try_find_iter(tag) {
        let attr = attr.map_err(|e| e.to_string())?;
        if attr.get(1) == Some(name) {
            let raw = attr.get(2).unwrap_or("");
            let value = &raw[1..raw.len() - 1];
            return Ok(Some(value.replace("&lt;", "<").replace("&gt;", ">")
                .replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")));
        }
    }
    Ok(None)
}

struct Control {
    name: String,
    cache: String,
    caption: String,
    attributes: String,
    inner: String,
    require_row_height: bool,
}

fn read_controls(xml: &str) -> Result<Vec<Control>, String> {
    let re = TryJsRegex::new(r#"<slicer\b[^>]*?(\/?)>"#, "").map_err(|e| e.to_string())?;
    let mut controls = Vec::new();
    let mut consumed = 0;
    for tag in re.try_find_iter(xml) {
        let tag = tag.map_err(|e| e.to_string())?;
        if tag.start() < consumed { continue; }
        let inner = if tag.get(1) == Some("/") {
            consumed = tag.end();
            ""
        } else if let Some(end) = xml[tag.end()..].find("</slicer>") {
            consumed = tag.end() + end + "</slicer>".len();
            &xml[tag.end()..tag.end() + end]
        } else {
            break;
        };
        let Some(name) = attribute(tag.as_str(), "name")? else { continue };
        let Some(cache) = attribute(tag.as_str(), "cache")? else { continue };
        let caption = attribute(tag.as_str(), "caption")?.unwrap_or_else(|| name.clone());
        let (attributes, _) = retained_attributes(tag.as_str(), &["name", "cache", "caption"])?;
        controls.push(Control { name, cache, caption, attributes, inner: inner.to_owned(), require_row_height: false });
    }
    Ok(controls)
}

fn serialize_control(control: &Control) -> String {
    let row_height = if control.require_row_height && !control.attributes.contains("rowHeight=") {
        " rowHeight=\"241300\""
    } else { "" };
    let start = format!("<slicer name=\"{}\" cache=\"{}\" caption=\"{}\"{}{}",
        escape_attribute(&control.name), escape_attribute(&control.cache), escape_attribute(&control.caption),
        control.attributes, row_height);
    if control.inner.is_empty() { format!("{start}/>") } else { format!("{start}>{}</slicer>", control.inner) }
}

/// Replace a single control while preserving other controls and their children.
pub(super) fn upsert_slicer_control(
    existing: Option<&str>, name: &str, cache: &str, caption: &str,
) -> Result<String, String> {
    let xml = existing.unwrap_or("");
    let root = first(xml, r#"<slicers\b[^>]*>"#)?.unwrap_or("");
    let (attributes, _) = retained_attributes(root, &["xmlns"])?;
    let mut controls = read_controls(xml)?;
    let previous = controls.iter().find(|control| control.name == name);
    let updated = Control {
        name: name.to_owned(), cache: cache.to_owned(), caption: caption.to_owned(),
        attributes: previous.map(|c| c.attributes.clone()).unwrap_or_default(),
        inner: previous.map(|c| c.inner.clone()).unwrap_or_default(), require_row_height: true,
    };
    if let Some(index) = controls.iter().position(|control| control.name == name) {
        controls[index] = updated;
    } else {
        controls.push(updated);
    }
    let content = controls.iter().map(serialize_control).collect::<String>();
    Ok(format!("{XML_DECL}<slicers xmlns=\"{X14_NS}\"{attributes}>{content}</slicers>"))
}

pub(super) fn remove_slicer_control(existing: &str, name: &str) -> Result<(String, bool), String> {
    let root = first(existing, r#"<slicers\b[^>]*>"#)?.unwrap_or("");
    let (attributes, _) = retained_attributes(root, &["xmlns"])?;
    let mut controls = read_controls(existing)?;
    controls.retain(|control| control.name != name);
    let empty = controls.is_empty();
    let content = controls.iter().map(serialize_control).collect::<String>();
    Ok((format!("{XML_DECL}<slicers xmlns=\"{X14_NS}\"{attributes}>{content}</slicers>"), empty))
}

pub(super) fn build_slicer_cache_xml(
    cache_name: &str,
    source_name: &str,
    tab_id: u64,
    pivot_name: &str,
    x14_pivot_cache_id: u64,
    items: &[SlicerItem],
    existing: Option<&str>,
) -> Result<String, String> {
    let existing = existing.unwrap_or("");
    let root = first(existing, r#"<slicerCacheDefinition\b[^>]*>"#)?.unwrap_or("");
    let (root_attrs, _) = retained_attributes(root, &["name", "sourceName", "xmlns", "xmlns:r"])?;
    let tabular = first(existing, r#"<tabular\b[^>]*>"#)?.unwrap_or("");
    let (mut tabular_attrs, has_cross_filter) = retained_attributes(tabular, &["pivotCacheId"])?;
    if !has_cross_filter { tabular_attrs.push_str(" crossFilter=\"showItemsWithDataAtTop\""); }
    let ext = match (existing.find("<extLst"), existing.rfind("</extLst>")) {
        (Some(start), Some(end)) if end > start => &existing[start..end + "</extLst>".len()],
        _ => "",
    };
    let pivot_tables = match first(existing, r#"<pivotTables\b[^>]*?(?:/>|>[\s\S]*?</pivotTables>)"#)? {
        Some(block) => block.to_owned(),
        None => format!("<pivotTables><pivotTable tabId=\"{tab_id}\" name=\"{}\"/></pivotTables>", escape_attribute(pivot_name)),
    };
    let mut item_xml = String::new();
    for item in items {
        item_xml.push_str(&format!("<i x=\"{}\"{}/>", item.x, if item.selected { " s=\"1\"" } else { "" }));
    }
    Ok(format!("{XML_DECL}<slicerCacheDefinition xmlns=\"{X14_NS}\" xmlns:r=\"{REL_NS}\"{root_attrs} name=\"{}\" sourceName=\"{}\">{pivot_tables}<data><tabular pivotCacheId=\"{x14_pivot_cache_id}\"{tabular_attrs}><items count=\"{}\">{item_xml}</items></tabular></data>{ext}</slicerCacheDefinition>", escape_attribute(cache_name), escape_attribute(source_name), items.len()))
}

pub(super) fn add_slicer_defined_name(xml: &str, cache_name: &str) -> Result<String, String> {
    let escaped = escape_attribute(cache_name);
    let element = format!("<definedName name=\"{escaped}\">#N/A</definedName>");
    let entries = TryJsRegex::new(r#"<definedName\b[^>]*\bname="([^"]*)"[^>]*>[^<]*</definedName>"#, "")
        .map_err(|e| e.to_string())?;
    for entry in entries.try_find_iter(xml) {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.get(1) == Some(escaped.as_str()) {
            return Ok(format!("{}{element}{}", &xml[..entry.start()], &xml[entry.end()..]));
        }
    }
    let empty = TryJsRegex::new(r#"<definedNames\b[^>]*/>"#, "").map_err(|e| e.to_string())?;
    if let Some(found) = empty.try_find_iter(xml).next().transpose().map_err(|e| e.to_string())? {
        return Ok(format!("{}<definedNames>{element}</definedNames>{}", &xml[..found.start()], &xml[found.end()..]));
    }
    if let Some(index) = xml.rfind("</definedNames>") {
        return Ok(format!("{}{element}{}", &xml[..index], &xml[index..]));
    }
    let sheets_end = xml.rfind("</sheets>").ok_or_else(|| "The workbook part is malformed.".to_owned())? + "</sheets>".len();
    let mut index = xml.rfind("</workbook>").ok_or_else(|| "The workbook part is malformed.".to_owned())?;
    for tag in ["<calcPr", "<oleSize", "<customWorkbookViews", "<pivotCaches", "<smartTagPr", "<smartTagTypes", "<webPublishing", "<fileRecoveryPr", "<webPublishObjects", "<extLst"] {
        if let Some(found) = xml[sheets_end..].find(tag) {
            index = index.min(sheets_end + found);
        }
    }
    Ok(format!("{}<definedNames>{element}</definedNames>{}", &xml[..index], &xml[index..]))
}

pub(super) fn remove_slicer_defined_name(xml: &str, cache_name: &str) -> Result<String, String> {
    let escaped = escape_attribute(cache_name);
    let entries = TryJsRegex::new(r#"<definedName\b[^>]*\bname="([^"]*)"[^>]*>[^<]*</definedName>\s*"#, "")
        .map_err(|e| e.to_string())?;
    let mut patched = xml.to_owned();
    for entry in entries.try_find_iter(xml) {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.get(1) == Some(escaped.as_str()) {
            patched = format!("{}{}", &xml[..entry.start()], &xml[entry.end()..]);
            break;
        }
    }
    if let Some(open) = patched.rfind("<definedNames>") {
        let content = open + "<definedNames>".len();
        if let Some(relative_close) = patched[content..].find("</definedNames>") {
            let close = content + relative_close;
            if patched[content..close].trim().is_empty() {
                patched.replace_range(open..close + "</definedNames>".len(), "");
            }
        }
    }
    Ok(patched)
}

struct ExtWiring {
    uri: &'static str,
    container: &'static str,
    item: &'static str,
}

const WORKBOOK_WIRING: ExtWiring = ExtWiring {
    uri: "{BBE1A952-AA13-448e-AADC-164F8A28A991}",
    container: "slicerCaches", item: "slicerCache",
};
const WORKSHEET_WIRING: ExtWiring = ExtWiring {
    uri: "{A8765BA9-456A-4dab-B4F3-ACF838C121DE}",
    container: "slicerList", item: "slicer",
};

fn add_ext_ref(xml: &str, root: &str, wiring: &ExtWiring, rel_id: &str) -> Result<String, String> {
    let entry = format!("<x14:{} r:id=\"{rel_id}\"/>", wiring.item);
    let container = format!("x14:{}", wiring.container);
    let ext_re = TryJsRegex::new(r#"<ext\b[^>]*>"#, "i").map_err(|e| e.to_string())?;
    for ext in ext_re.try_find_iter(xml) {
        let ext = ext.map_err(|e| e.to_string())?;
        if attribute(ext.as_str(), "uri")?.as_deref().map(str::to_ascii_lowercase) != Some(wiring.uri.to_ascii_lowercase()) { continue; }
        let end = xml[ext.end()..].find("</ext>").map(|p| p + ext.end())
            .ok_or_else(|| "A slicer ext element is malformed.".to_owned())?;
        let block = &xml[ext.start()..end];
        if block.contains(&format!("r:id=\"{rel_id}\"")) { return Ok(xml.to_owned()); }
        let close = format!("</{container}>");
        let patched = if let Some(at) = block.rfind(&close) {
            format!("{}{entry}{}", &block[..at], &block[at..])
        } else {
            format!("{block}<{container}>{entry}</{container}>")
        };
        return Ok(format!("{}{patched}{}", &xml[..ext.start()], &xml[end..]));
    }
    let element = format!("<ext uri=\"{}\" xmlns:x14=\"{X14_NS}\" xmlns:r=\"{REL_NS}\"><{container}>{entry}</{container}></ext>", wiring.uri);
    insert_root_ext(xml, root, &element)
}

fn remove_ext_ref(xml: &str, wiring: &ExtWiring, rel_id: &str) -> Result<String, String> {
    let exts = TryJsRegex::new(r#"<ext\b[^>]*>"#, "i").map_err(|e| e.to_string())?;
    for ext in exts.try_find_iter(xml) {
        let ext = ext.map_err(|e| e.to_string())?;
        if attribute(ext.as_str(), "uri")?.as_deref().map(str::to_ascii_lowercase) != Some(wiring.uri.to_ascii_lowercase()) { continue; }
        let end = xml[ext.end()..].find("</ext>").map(|p| p + ext.end())
            .ok_or_else(|| "A slicer ext element is malformed.".to_owned())?;
        let block = &xml[ext.start()..end];
        let item_re = TryJsRegex::new(&format!(r#"<x14:{}\b[^>]*/>\s*"#, wiring.item), "").map_err(|e| e.to_string())?;
        let mut stripped = None;
        for item in item_re.try_find_iter(block) {
            let item = item.map_err(|e| e.to_string())?;
            if attribute(item.as_str(), "r:id")?.as_deref() == Some(rel_id) {
                stripped = Some(format!("{}{}", &block[..item.start()], &block[item.end()..]));
                break;
            }
        }
        let Some(mut inner) = stripped else { return Ok(xml.to_owned()) };
        let open_re = TryJsRegex::new(&format!(r#"<x14:{}\b[^>]*>"#, wiring.container), "").map_err(|e| e.to_string())?;
        let open_at = open_re.try_find_iter(&inner).next().transpose().map_err(|e| e.to_string())?.map(|found| found.start());
        if let Some(open) = open_at {
            let close_tag = format!("</x14:{}>", wiring.container);
            if let Some(close) = inner.rfind(&close_tag) {
                let item_tag = format!("<x14:{}", wiring.item);
                if !inner[open..close].contains(&item_tag) {
                    inner = format!("{}{}", &inner[..open], &inner[close + close_tag.len()..]);
                }
            }
        }
        let content_start = ext.end() - ext.start();
        let replacement = if inner[content_start..].trim().is_empty() { String::new() } else { format!("{inner}</ext>") };
        let mut patched = format!("{}{replacement}{}", &xml[..ext.start()], &xml[end + "</ext>".len()..]);
        if replacement.is_empty() {
            if let Some(close) = patched.rfind("</extLst>") {
                if let Some(open) = patched[..close].rfind("<extLst>") {
                    if patched[open + "<extLst>".len()..close].trim().is_empty() {
                        patched.replace_range(open..close + "</extLst>".len(), "");
                    }
                }
            }
        }
        return Ok(patched);
    }
    Ok(xml.to_owned())
}

fn insert_root_ext(xml: &str, root: &str, element: &str) -> Result<String, String> {
    let tags = TryJsRegex::new(r#"<!--[\s\S]*?-->|<!\[CDATA\[[\s\S]*?\]\]>|<\?[\s\S]*?\?>|<!DOCTYPE\b[\s\S]*?>|</?[A-Za-z_][\w:.-]*(?:[^>"']|"[^"]*"|'[^']*')*/?>"#, "")
        .map_err(|e| e.to_string())?;
    let name = TryJsRegex::new(r#"^</?([^\s/>]+)"#, "").map_err(|e| e.to_string())?;
    let mut stack: Vec<String> = Vec::new();
    let mut ext: Option<(usize, usize, bool)> = None;
    let mut ext_open = None;
    for token in tags.try_find_iter(xml) {
        let token = token.map_err(|e| e.to_string())?;
        let tag = token.as_str();
        if tag.starts_with("<!--") || tag.starts_with("<![CDATA[") || tag.starts_with("<?") { continue; }
        if tag.starts_with("<!") { return Err("The host element is malformed.".into()); }
        let matched = name.try_find_iter(tag).next().transpose().map_err(|e| e.to_string())?;
        let Some(tag_name) = matched.as_ref().and_then(|m| m.get(1)) else { return Err("The host element is malformed.".into()); };
        if tag.starts_with("</") {
            if stack.pop().as_deref() != Some(tag_name) { return Err("The host element is malformed.".into()); }
            if stack.len() == 1 && tag_name == "extLst" {
                if let Some(open) = ext_open { ext = Some((open, token.start(), false)); }
            }
            if stack.is_empty() {
                return match ext {
                    None => Ok(format!("{}<extLst>{element}</extLst>{}", &xml[..token.start()], &xml[token.start()..])),
                    Some((open, close, true)) => Ok(format!("{}<extLst>{element}</extLst>{}", &xml[..open], &xml[close..])),
                    Some((_, close, false)) => Ok(format!("{}{element}{}", &xml[..close], &xml[close..])),
                };
            }
        } else {
            if stack.is_empty() && tag_name != root { return Err("The host element is malformed.".into()); }
            if stack.len() == 1 && tag_name == "extLst" {
                if tag.ends_with("/>") { ext = Some((token.start(), token.end(), true)); }
                else { ext_open = Some(token.start()); }
            }
            if !tag.ends_with("/>") { stack.push(tag_name.to_owned()); }
        }
    }
    Err("The host element is malformed.".into())
}

fn add_control_frame_xml(xml: &str, name: &str) -> Result<String, String> {
    let slicers = TryJsRegex::new(r#"<sle:slicer\b[^>]*>"#, "").map_err(|e| e.to_string())?;
    for tag in slicers.try_find_iter(xml) {
        let tag = tag.map_err(|e| e.to_string())?;
        if tag.as_str().contains(&format!("name=\"{name}\"")) { return Ok(xml.to_owned()); }
    }
    let anchors = TryJsRegex::new(r#"<xdr:twoCellAnchor\b"#, "").map_err(|e| e.to_string())?;
    let mut count = 0_u64;
    for anchor in anchors.try_find_iter(xml) { anchor.map_err(|e| e.to_string())?; count += 1; }
    let col = 8 + (count % 4) * 4;
    let row = 2 + (count / 4) * 14;
    let marker = |col, row| format!("<xdr:col>{col}</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{row}</xdr:row><xdr:rowOff>0</xdr:rowOff>");
    let ids = TryJsRegex::new(r#"<xdr:cNvPr\b[^>]*\bid="(\d+)""#, "").map_err(|e| e.to_string())?;
    let mut max = 1_u64;
    for id in ids.try_find_iter(xml) {
        let id = id.map_err(|e| e.to_string())?;
        if let Some(id) = id.get(1).and_then(|s| s.parse::<u64>().ok()) { max = max.max(id); }
    }
    let id = max.checked_add(1).ok_or_else(|| "Drawing id overflow.".to_owned())?;
    let fallback = max.checked_add(2).ok_or_else(|| "Drawing id overflow.".to_owned())?;
    let escaped = escape_attribute(name);
    let frame = format!(concat!(
        "<xdr:twoCellAnchor editAs=\"oneCell\"><xdr:from>{}</xdr:from><xdr:to>{}</xdr:to>",
        "<mc:AlternateContent xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\">",
        "<mc:Choice xmlns:sle=\"http://schemas.microsoft.com/office/drawing/2010/slicer\" Requires=\"sle\">",
        "<xdr:graphicFrame macro=\"\"><xdr:nvGraphicFramePr><xdr:cNvPr id=\"{id}\" name=\"{escaped}\"/>",
        "<xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/></xdr:xfrm>",
        "<a:graphic><a:graphicData uri=\"http://schemas.microsoft.com/office/drawing/2010/slicer\">",
        "<sle:slicer xmlns:sle=\"http://schemas.microsoft.com/office/drawing/2010/slicer\" name=\"{escaped}\"/>",
        "</a:graphicData></a:graphic></xdr:graphicFrame></mc:Choice>",
        "<mc:Fallback><xdr:sp macro=\"\" textlink=\"\"><xdr:nvSpPr><xdr:cNvPr id=\"{fallback}\" name=\"{escaped} Fallback\"/>",
        "<xdr:cNvSpPr/><xdr:nvPr/></xdr:nvSpPr><xdr:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/></a:xfrm>",
        "<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:noFill/><a:ln><a:noFill/></a:ln></xdr:spPr>",
        "<xdr:txBody><a:bodyPr/><a:lstStyle/><a:p><a:endParaRPr lang=\"en-US\"/></a:p></xdr:txBody>",
        "</xdr:sp></mc:Fallback></mc:AlternateContent><xdr:clientData/></xdr:twoCellAnchor>"
    ), marker(col, row), marker(col + 3, row + 12), id=id, escaped=escaped, fallback=fallback);
    Ok(xml.replacen("</xdr:wsDr>", &format!("{frame}</xdr:wsDr>"), 1))
}

pub(super) fn add_control_frame_to_drawing(
    pkg: &mut PackageEditor<'_>, drawing_path: &str, name: &str, touched: &mut HashSet<String>,
) -> Result<(), String> {
    let xml = pkg.read_text(drawing_path)?;
    let updated = add_control_frame_xml(&xml, name)?;
    if updated != xml.as_str() {
        pkg.write(drawing_path, updated)?;
        touched.insert(drawing_path.to_owned());
    }
    Ok(())
}

pub(super) fn ensure_sheet_drawing(pkg: &mut PackageEditor<'_>, sheet: &str, touched: &mut HashSet<String>) -> Result<String, String> {
    let rels = rels_path_for(sheet);
    let kind = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing";
    if pkg.has(&rels)? {
        let xml = pkg.read_text(&rels)?;
        let tags = TryJsRegex::new(r#"<Relationship\b[^>]*>"#, "").map_err(|e| e.to_string())?;
        for tag in tags.try_find_iter(&xml) {
            let tag = tag.map_err(|e| e.to_string())?;
            if tag.as_str().contains(&format!("Type=\"{kind}\"")) {
                if let Some(target) = attribute(tag.as_str(), "Target")? {
                    if attribute(tag.as_str(), "Id")?.is_some() {
                        return super::state::resolve_rel_target(sheet, &target);
                    }
                }
            }
        }
    }
    let path = super::pivots::allocate_part_path(pkg, "xl/drawings/drawing", ".xml")?;
    pkg.add(&path, "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<xdr:wsDr xmlns:xdr=\"http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"></xdr:wsDr>".to_owned());
    touched.insert(path.clone());
    super::pivots::register_content_type_override(pkg, &path, "application/vnd.openxmlformats-officedocument.drawing+xml", touched)?;
    let rel_id = append_relationship(pkg, &rels, kind, &relative_target(sheet, &path), false)?;
    touched.insert(rels);
    let xml = pkg.read_text(sheet)?;
    let drawing = TryJsRegex::new(r#"<drawing[\s/>]"#, "").map_err(|e| e.to_string())?;
    if drawing.try_test(&xml).map_err(|e| e.to_string())? {
        return Err(format!("{sheet} already has a drawing element but no drawing rel."));
    }
    let root = TryJsRegex::new(r#"<(?![?!])(?:[^>"']|"[^"]*"|'[^']*')*?/?>"#, "").map_err(|e| e.to_string())?;
    let first = root.try_find_iter(&xml).next().transpose().map_err(|e| e.to_string())?;
    let mut sheet_xml = if let Some(root) = first {
        let tag = root.as_str();
        if TryJsRegex::new(r#"\bxmlns:r\s*="#, "").map_err(|e| e.to_string())?.try_test(tag).map_err(|e| e.to_string())? {
            xml.as_str().to_owned()
        } else {
            let end = tag.find(|c: char| c.is_whitespace() || c == '/' || c == '>').unwrap_or(tag.len());
            format!("{}{} xmlns:r=\"{REL_NS}\"{}{}", &xml[..root.start()], &tag[..end], &tag[end..], &xml[root.end()..])
        }
    } else { xml.as_str().to_owned() };
    let mut index = sheet_xml.rfind("</worksheet>").ok_or_else(|| format!("{sheet} has no closing worksheet tag."))?;
    for name in ["legacyDrawing", "legacyDrawingHF", "picture", "oleObjects", "controls", "webPublishItems", "tableParts", "extLst"] {
        let re = TryJsRegex::new(&format!(r#"<{name}[\s/>]"#), "").map_err(|e| e.to_string())?;
        if let Some(found) = re.try_find_iter(&sheet_xml).next().transpose().map_err(|e| e.to_string())? { index = index.min(found.start()); }
    }
    sheet_xml.insert_str(index, &format!("<drawing r:id=\"{}\"/>", escape_attribute(&rel_id)));
    pkg.write(sheet, sheet_xml)?;
    touched.insert(sheet.to_owned());
    Ok(path)
}

pub(super) fn ensure_control_frame(
    pkg: &mut PackageEditor<'_>, sheet: &str, name: &str, touched: &mut HashSet<String>,
) -> Result<(), String> {
    let drawing = ensure_sheet_drawing(pkg, sheet, touched)?;
    add_control_frame_to_drawing(pkg, &drawing, name, touched)
}

pub(super) fn remove_control_frame(
    pkg: &mut PackageEditor<'_>, worksheet: &str, name: &str, touched: &mut HashSet<String>,
) -> Result<(), String> {
    let rels = rels_path_for(worksheet);
    if !pkg.has(&rels)? { return Ok(()); }
    let xml = pkg.read_text(&rels)?;
    let tags = TryJsRegex::new(r#"<Relationship\b[^>]*>"#, "").map_err(|e| e.to_string())?;
    let mut drawing = None;
    for tag in tags.try_find_iter(&xml) {
        let tag = tag.map_err(|e| e.to_string())?;
        if attribute(tag.as_str(), "Type")?.as_deref() == Some("http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing") {
            if let Some(target) = attribute(tag.as_str(), "Target")? { drawing = Some(super::state::resolve_rel_target(worksheet, &target)?); }
            break;
        }
    }
    let Some(drawing) = drawing else { return Ok(()) };
    if !pkg.has(&drawing)? { return Ok(()); }
    let xml = pkg.read_text(&drawing)?;
    let anchors = TryJsRegex::new(r#"<xdr:twoCellAnchor\b[^>]*>[\s\S]*?</xdr:twoCellAnchor>\s*"#, "").map_err(|e| e.to_string())?;
    let tags = TryJsRegex::new(r#"<sle:slicer\b[^>]*/>"#, "").map_err(|e| e.to_string())?;
    for anchor in anchors.try_find_iter(&xml) {
        let anchor = anchor.map_err(|e| e.to_string())?;
        for tag in tags.try_find_iter(anchor.as_str()) {
            let tag = tag.map_err(|e| e.to_string())?;
            if tag.as_str().contains(&format!("name=\"{name}\"")) {
                pkg.write(&drawing, format!("{}{}", &xml[..anchor.start()], &xml[anchor.end()..]))?;
                touched.insert(drawing);
                return Ok(());
            }
        }
    }
    Ok(())
}

fn append_or_reuse_relationship(
    pkg: &mut PackageEditor<'_>, rels_path: &str, kind: &str, from: &str, part: &str,
    touched: &mut HashSet<String>,
) -> Result<String, String> {
    if pkg.has(rels_path)? {
        let xml = pkg.read_text(rels_path)?;
        let tags = TryJsRegex::new(r#"<Relationship\b[^>]*>"#, "").map_err(|e| e.to_string())?;
        for tag in tags.try_find_iter(&xml) {
            let tag = tag.map_err(|e| e.to_string())?;
            if attribute(tag.as_str(), "Type")?.as_deref() != Some(kind) { continue; }
            let Some(id) = attribute(tag.as_str(), "Id")? else { continue };
            let Some(target) = attribute(tag.as_str(), "Target")? else { continue };
            if super::state::resolve_rel_target(from, &target)? == part { return Ok(id); }
        }
    }
    let id = append_relationship(pkg, rels_path, kind, &relative_target(from, part), false)?;
    touched.insert(rels_path.to_owned());
    Ok(id)
}

fn ensure_worksheet_ref(
    pkg: &mut PackageEditor<'_>, worksheet: &str, part: &str, touched: &mut HashSet<String>,
) -> Result<(), String> {
    let rels = rels_path_for(worksheet);
    let id = append_or_reuse_relationship(pkg, &rels, "http://schemas.microsoft.com/office/2007/relationships/slicer", worksheet, part, touched)?;
    let xml = pkg.read_text(worksheet)?;
    let updated = add_ext_ref(&xml, "worksheet", &WORKSHEET_WIRING, &id)?;
    if updated != xml.as_str() {
        pkg.write(worksheet, updated)?;
        touched.insert(worksheet.to_owned());
    }
    Ok(())
}

const CACHE_EXT_URI: &str = "{725AE2AE-9491-48be-B2B4-4EB974FC3084}";

fn cache_definition_paths(pkg: &mut PackageEditor<'_>, workbook: &str) -> Result<Vec<(u64, String)>, String> {
    let rels = if pkg.has("xl/_rels/workbook.xml.rels")? {
        Some(pkg.read_text("xl/_rels/workbook.xml.rels")?)
    } else { None };
    let Some(rels) = rels else { return Ok(Vec::new()) };
    let caches = TryJsRegex::new(r#"<pivotCache\b[^>]*>"#, "").map_err(|e| e.to_string())?;
    let rel_tags = TryJsRegex::new(r#"<Relationship\b[^>]*>"#, "").map_err(|e| e.to_string())?;
    let mut result = Vec::new();
    for cache in caches.try_find_iter(workbook) {
        let cache = cache.map_err(|e| e.to_string())?;
        let Some(id) = attribute(cache.as_str(), "cacheId")?.and_then(|s| s.parse::<u64>().ok()) else { continue };
        let Some(rel_id) = attribute(cache.as_str(), "r:id")? else { continue };
        for rel in rel_tags.try_find_iter(&rels) {
            let rel = rel.map_err(|e| e.to_string())?;
            if attribute(rel.as_str(), "Id")?.as_deref() != Some(rel_id.as_str()) { continue; }
            if let Some(target) = attribute(rel.as_str(), "Target")? {
                result.push((id, super::state::resolve_rel_target("xl/workbook.xml", &target)?));
            }
            break;
        }
    }
    Ok(result)
}

fn x14_cache_id(xml: &str) -> Result<Option<u64>, String> {
    let exts = TryJsRegex::new(r#"<ext\b[^>]*>[\s\S]*?</ext>"#, "").map_err(|e| e.to_string())?;
    for ext in exts.try_find_iter(xml) {
        let ext = ext.map_err(|e| e.to_string())?;
        if attribute(ext.as_str().split('>').next().unwrap_or(""), "uri")?.as_deref() != Some(CACHE_EXT_URI) { continue; }
        if let Some(tag) = first(ext.as_str(), r#"<x14:pivotCacheDefinition\b[^>]*>"#)? {
            return Ok(attribute(tag, "pivotCacheId")?.and_then(|value| value.parse::<u64>().ok()).filter(|id| *id > 0));
        }
        return Ok(None);
    }
    Ok(None)
}

pub(super) fn ensure_pivot_cache_slicer_ext(
    pkg: &mut PackageEditor<'_>, workbook: &str, cache_id: u64, touched: &mut HashSet<String>,
) -> Result<u64, String> {
    let paths = cache_definition_paths(pkg, workbook)?;
    let path = paths.iter().find(|(id, _)| *id == cache_id).map(|(_, path)| path.clone())
        .ok_or_else(|| format!("No pivotCacheDefinition carries cacheId {cache_id}."))?;
    let xml = pkg.read_text(&path)?;
    let id = if let Some(id) = x14_cache_id(&xml)? { id } else {
        let mut max = 0_u64;
        for (_, path) in &paths {
            if pkg.has(path)? { max = max.max(x14_cache_id(&pkg.read_text(path)?)?.unwrap_or(0)); }
        }
        max.checked_add(1).ok_or_else(|| "Slicer cache id overflow.".to_owned())?
    };
    let element = format!("<ext uri=\"{CACHE_EXT_URI}\" xmlns:x14=\"{X14_NS}\"><x14:pivotCacheDefinition pivotCacheId=\"{id}\"/></ext>");
    let ext_re = TryJsRegex::new(r#"<ext\b[^>]*>[\s\S]*?</ext>"#, "").map_err(|e| e.to_string())?;
    let mut existing = None;
    for ext in ext_re.try_find_iter(&xml) {
        let ext = ext.map_err(|e| e.to_string())?;
        if attribute(ext.as_str().split('>').next().unwrap_or(""), "uri")?.as_deref() == Some(CACHE_EXT_URI) {
            existing = Some((ext.start(), ext.end())); break;
        }
    }
    let patched = if let Some((start, end)) = existing {
        format!("{}{element}{}", &xml[..start], &xml[end..])
    } else if xml.contains("</pivotCacheDefinition>") {
        insert_root_ext(&xml, "pivotCacheDefinition", &element).map_err(|_| format!("Pivot cache definition {path} is malformed."))?
    } else {
        let self_closing = TryJsRegex::new(r#"<pivotCacheDefinition\b[^>]*?/>"#, "").map_err(|e| e.to_string())?;
        let tag = self_closing.try_find_iter(&xml).next().transpose().map_err(|e| e.to_string())?
            .ok_or_else(|| format!("Pivot cache definition {path} is malformed."))?;
        format!("{}{}><extLst>{element}</extLst></pivotCacheDefinition>{}", &xml[..tag.start()], &tag.as_str()[..tag.as_str().len()-2], &xml[tag.end()..])
    };
    pkg.write(&path, patched)?;
    touched.insert(path);
    Ok(id)
}

fn relationship_target(xml: &str, id: &str) -> Result<Option<String>, String> {
    let tags = TryJsRegex::new(r#"<Relationship\b[^>]*>"#, "").map_err(|e| e.to_string())?;
    for tag in tags.try_find_iter(xml) {
        let tag = tag.map_err(|e| e.to_string())?;
        if attribute(tag.as_str(), "Id")?.as_deref() == Some(id) {
            return attribute(tag.as_str(), "Target");
        }
    }
    Ok(None)
}

fn pivot_binding(pkg: &mut PackageEditor<'_>, workbook: &str, pivot: &str) -> Result<(u64, String, u64), String> {
    if !pkg.has(pivot)? { return Err(format!("Slicer pivot part {pivot} is missing from the workbook.")); }
    let pivot_xml = pkg.read_text(pivot)?;
    let name = attribute(&pivot_xml, "name")?.ok_or_else(|| format!("Pivot part {pivot} has no pivot table name."))?;
    let cache_id = attribute(&pivot_xml, "cacheId")?.and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| format!("Pivot part {pivot} has no cacheId."))?;
    let relationship_tags = TryJsRegex::new(r#"<Relationship\b[^>]*>"#, "").map_err(|e| e.to_string())?;
    let mut host = None;
    for path in pkg.paths()? {
        let Some(name) = path.strip_prefix("xl/worksheets/") else { continue };
        if name.contains('/') || !name.ends_with(".xml") { continue; }
        let rels = rels_path_for(&path);
        if !pkg.has(&rels)? { continue; }
        let xml = pkg.read_text(&rels)?;
        for tag in relationship_tags.try_find_iter(&xml) {
            let tag = tag.map_err(|e| e.to_string())?;
            if attribute(tag.as_str(), "Type")?.as_deref() != Some("http://schemas.openxmlformats.org/officeDocument/2006/relationships/pivotTable") { continue; }
            let Some(target) = attribute(tag.as_str(), "Target")? else { continue };
            if super::state::resolve_rel_target(&path, &target)? == pivot {
                host = Some(path.clone()); break;
            }
        }
        if host.is_some() { break; }
    }
    let host = host.ok_or_else(|| format!("Pivot part {pivot} is not hosted by any worksheet."))?;
    let workbook_rels = if pkg.has("xl/_rels/workbook.xml.rels")? { Some(pkg.read_text("xl/_rels/workbook.xml.rels")?) } else { None };
    let sheets = TryJsRegex::new(r#"<sheet\b[^>]*>"#, "").map_err(|e| e.to_string())?;
    for sheet in sheets.try_find_iter(workbook) {
        let sheet = sheet.map_err(|e| e.to_string())?;
        let Some(id) = attribute(sheet.as_str(), "sheetId")? else { continue };
        let Some(rel_id) = attribute(sheet.as_str(), "r:id")? else { continue };
        let Some(rels) = &workbook_rels else { continue };
        let Some(target) = relationship_target(rels, &rel_id)? else { continue };
        if super::state::resolve_rel_target("xl/workbook.xml", &target)? != host { continue; }
        let tab_id = id.parse::<u64>().ok().filter(|id| *id > 0)
            .ok_or_else(|| format!("Sheet {host} carries no usable sheetId."))?;
        return Ok((tab_id, name, cache_id));
    }
    Err(format!("The workbook does not list a sheet for {host}."))
}

pub(super) struct SlicerControl<'a> {
    pub worksheet_path: &'a str,
    pub pivot_path: &'a str,
    pub source_name: &'a str,
    pub caption: &'a str,
    pub name: Option<&'a str>,
    pub part_path: Option<&'a str>,
    pub cache_path: Option<&'a str>,
    pub cache_name: Option<&'a str>,
    pub items: &'a [SlicerItem],
}

fn sanitized_name(value: &str) -> String {
    let mut result: String = value.encode_utf16().map(|unit| {
        if unit <= 127 && ((unit as u8).is_ascii_alphanumeric() || unit == b'_' as u16) {
            unit as u8 as char
        } else { '_' }
    }).collect();
    if result.starts_with(|ch: char| ch.is_ascii_digit() || ch == '_') { result.insert(0, 'S'); }
    result.truncate(200);
    if result.is_empty() { "Field".to_owned() } else { result }
}

fn unique_name(base: &str, taken: &mut HashSet<String>) -> String {
    if taken.insert(base.to_owned()) { return base.to_owned(); }
    let mut index = 2_u64;
    loop {
        let candidate = format!("{base}_{index}");
        if taken.insert(candidate.clone()) { return candidate; }
        index += 1;
    }
}

fn existing_slicer_part(pkg: &mut PackageEditor<'_>, sheet: &str) -> Result<Option<String>, String> {
    let rels = rels_path_for(sheet);
    if !pkg.has(&rels)? { return Ok(None); }
    let xml = pkg.read_text(&rels)?;
    let tags = TryJsRegex::new(r#"<Relationship\b[^>]*>"#, "").map_err(|e| e.to_string())?;
    for tag in tags.try_find_iter(&xml) {
        let tag = tag.map_err(|e| e.to_string())?;
        if attribute(tag.as_str(), "Type")?.as_deref() != Some("http://schemas.microsoft.com/office/2007/relationships/slicer") { continue; }
        let Some(target) = attribute(tag.as_str(), "Target")? else { continue };
        let path = super::state::resolve_rel_target(sheet, &target)?;
        if pkg.has(&path)? { return Ok(Some(path)); }
    }
    Ok(None)
}

pub(super) fn collect_slicer_names(pkg: &mut PackageEditor<'_>) -> Result<(HashSet<String>, HashSet<String>), String> {
    let mut controls = HashSet::new();
    let mut caches = HashSet::new();
    for path in pkg.paths()? {
        if !path.ends_with(".xml") { continue; }
        if path.starts_with("xl/slicers/") {
            for control in read_controls(&pkg.read_text(&path)?)? { controls.insert(control.name); }
        } else if path.starts_with("xl/slicerCaches/") {
            let xml = pkg.read_text(&path)?;
            if let Some(tag) = first(&xml, r#"<slicerCacheDefinition\b[^>]*>"#)? {
                if let Some(name) = attribute(tag, "name")? { caches.insert(name); }
            }
        }
    }
    Ok((controls, caches))
}

pub(super) fn write_slicer<F>(
    pkg: &mut PackageEditor<'_>, workbook: &str, control: &SlicerControl<'_>, touched: &mut HashSet<String>,
    taken_controls: &mut HashSet<String>, taken_caches: &mut HashSet<String>, ensure_frame: F,
) -> Result<String, String>
where F: FnOnce(&mut PackageEditor<'_>, &str, &str, &mut HashSet<String>) -> Result<(), String> {
    if !pkg.has(control.worksheet_path)? {
        return Err(format!("Slicer host worksheet {} is missing.", control.worksheet_path));
    }
    let (tab_id, pivot_name, workbook_cache_id) = pivot_binding(pkg, workbook, control.pivot_path)?;
    let x14_cache_id = ensure_pivot_cache_slicer_ext(pkg, workbook, workbook_cache_id, touched)?;
    let part_path = if let Some(path) = control.part_path { path.to_owned() }
        else if let Some(path) = existing_slicer_part(pkg, control.worksheet_path)? { path }
        else { super::pivots::allocate_part_path(pkg, "xl/slicers/slicer", ".xml")? };
    let cache_path = if let Some(path) = control.cache_path { path.to_owned() }
        else { super::pivots::allocate_part_path(pkg, "xl/slicerCaches/slicerCache", ".xml")? };
    let cache_name = control.cache_name.map(str::to_owned)
        .unwrap_or_else(|| unique_name(&format!("Slicer_{}", sanitized_name(control.source_name)), taken_caches));
    let name = control.name.map(str::to_owned)
        .unwrap_or_else(|| unique_name(&sanitized_name(if control.caption.is_empty() { control.source_name } else { control.caption }), taken_controls));
    materialize_slicer(pkg, workbook, &ResolvedSlicer {
        worksheet_path: control.worksheet_path, part_path: &part_path, cache_path: &cache_path,
        name: &name, cache_name: &cache_name, source_name: control.source_name, caption: control.caption,
        pivot_name: &pivot_name, pivot_sheet_id: tab_id, x14_cache_id, items: control.items,
    }, touched, ensure_frame)
}

pub(super) struct ResolvedSlicer<'a> {
    pub worksheet_path: &'a str,
    pub part_path: &'a str,
    pub cache_path: &'a str,
    pub name: &'a str,
    pub cache_name: &'a str,
    pub source_name: &'a str,
    pub caption: &'a str,
    pub pivot_name: &'a str,
    pub pivot_sheet_id: u64,
    pub x14_cache_id: u64,
    pub items: &'a [SlicerItem],
}

/// Write a fully resolved control and its cache, including package links.
pub(super) fn materialize_slicer<F>(
    pkg: &mut PackageEditor<'_>, workbook_xml: &str, control: &ResolvedSlicer<'_>,
    touched: &mut HashSet<String>, ensure_frame: F,
) -> Result<String, String>
where F: FnOnce(&mut PackageEditor<'_>, &str, &str, &mut HashSet<String>) -> Result<(), String> {
    if !pkg.has(control.worksheet_path)? {
        return Err(format!("Slicer host worksheet {} is missing.", control.worksheet_path));
    }
    let old_part = if pkg.has(control.part_path)? { Some(pkg.read_text(control.part_path)?) } else { None };
    let old_cache = if pkg.has(control.cache_path)? { Some(pkg.read_text(control.cache_path)?) } else { None };
    let cache = build_slicer_cache_xml(control.cache_name, control.source_name, control.pivot_sheet_id,
        control.pivot_name, control.x14_cache_id, control.items, old_cache.as_deref().map(String::as_str))?;
    if old_cache.is_some() { pkg.write(control.cache_path, cache)?; } else { pkg.add(control.cache_path, cache); }
    touched.insert(control.cache_path.to_owned());
    super::pivots::register_content_type_override(pkg, control.cache_path, "application/vnd.ms-excel.slicerCache+xml", touched)?;

    let workbook_rels = "xl/_rels/workbook.xml.rels";
    let id = append_or_reuse_relationship(pkg, workbook_rels,
        "http://schemas.microsoft.com/office/2007/relationships/slicerCache",
        "xl/workbook.xml", control.cache_path, touched)?;
    let workbook = add_ext_ref(workbook_xml, "workbook", &WORKBOOK_WIRING, &id)?;
    let workbook = add_slicer_defined_name(&workbook, control.cache_name)?;

    let part = upsert_slicer_control(old_part.as_deref().map(String::as_str), control.name, control.cache_name, control.caption)?;
    if old_part.is_some() { pkg.write(control.part_path, part)?; } else { pkg.add(control.part_path, part); }
    touched.insert(control.part_path.to_owned());
    super::pivots::register_content_type_override(pkg, control.part_path, "application/vnd.ms-excel.slicer+xml", touched)?;
    ensure_worksheet_ref(pkg, control.worksheet_path, control.part_path, touched)?;
    ensure_frame(pkg, control.worksheet_path, control.name, touched)?;
    Ok(workbook)
}

fn find_relationship(xml: &str, kind: &str, from: &str, part: &str) -> Result<Option<String>, String> {
    let tags = TryJsRegex::new(r#"<Relationship\b[^>]*>"#, "").map_err(|e| e.to_string())?;
    for tag in tags.try_find_iter(xml) {
        let tag = tag.map_err(|e| e.to_string())?;
        if attribute(tag.as_str(), "Type")?.as_deref() != Some(kind) { continue; }
        let Some(id) = attribute(tag.as_str(), "Id")? else { continue };
        let Some(target) = attribute(tag.as_str(), "Target")? else { continue };
        if super::state::resolve_rel_target(from, &target)? == part { return Ok(Some(id)); }
    }
    Ok(None)
}

fn remove_relationship(xml: &str, id: &str) -> Result<String, String> {
    let tags = TryJsRegex::new(r#"<Relationship\b[^>]*/>\s*"#, "").map_err(|e| e.to_string())?;
    for tag in tags.try_find_iter(xml) {
        let tag = tag.map_err(|e| e.to_string())?;
        if attribute(tag.as_str(), "Id")?.as_deref() == Some(id) && tag.as_str().contains(&format!("Id=\"{id}\"")) {
            return Ok(format!("{}{}", &xml[..tag.start()], &xml[tag.end()..]));
        }
    }
    Ok(xml.to_owned())
}

fn remove_part(pkg: &mut PackageEditor<'_>, path: &str, touched: &mut HashSet<String>) -> Result<(), String> {
    if !pkg.has(path)? { return Ok(()); }
    pkg.remove(path);
    touched.insert(path.to_owned());
    let rels = rels_path_for(path);
    if pkg.has(&rels)? { pkg.remove(&rels); touched.insert(rels); }
    let types = "[Content_Types].xml";
    let xml = pkg.read_text(types)?;
    let overrides = TryJsRegex::new(r#"<Override\b[^>]*/>\s*"#, "").map_err(|e| e.to_string())?;
    for tag in overrides.try_find_iter(&xml) {
        let tag = tag.map_err(|e| e.to_string())?;
        if tag.as_str().contains(&format!("PartName=\"/{path}\"")) {
            pkg.write(types, format!("{}{}", &xml[..tag.start()], &xml[tag.end()..]))?;
            touched.insert(types.to_owned());
            break;
        }
    }
    Ok(())
}

fn remove_pivot_cache_slicer_ext(
    pkg: &mut PackageEditor<'_>, workbook: &str, bound_id: u64, touched: &mut HashSet<String>,
) -> Result<(), String> {
    for path in pkg.paths()? {
        let Some(name) = path.strip_prefix("xl/slicerCaches/") else { continue };
        if name.contains('/') || !name.ends_with(".xml") { continue; }
        let xml = pkg.read_text(&path)?;
        let tag = first(&xml, r#"<tabular\b[^>]*>"#)?.unwrap_or("");
        if attribute(tag, "pivotCacheId")?.and_then(|value| value.parse::<u64>().ok()) == Some(bound_id) { return Ok(()); }
    }
    for (_, path) in cache_definition_paths(pkg, workbook)? {
        if !pkg.has(&path)? { continue; }
        let xml = pkg.read_text(&path)?;
        if x14_cache_id(&xml)? != Some(bound_id) { continue; }
        let exts = TryJsRegex::new(r#"<ext\b[^>]*>[\s\S]*?</ext>\s*"#, "").map_err(|e| e.to_string())?;
        let mut patched = xml.as_str().to_owned();
        for ext in exts.try_find_iter(&xml) {
            let ext = ext.map_err(|e| e.to_string())?;
            if attribute(ext.as_str().split('>').next().unwrap_or(""), "uri")?.as_deref() == Some(CACHE_EXT_URI) {
                patched = format!("{}{}", &xml[..ext.start()], &xml[ext.end()..]);
                break;
            }
        }
        if let Some(open) = patched.rfind("<extLst>") {
            let start = open + "<extLst>".len();
            if let Some(relative_close) = patched[start..].find("</extLst>") {
                let close = start + relative_close;
                if patched[start..close].trim().is_empty() {
                    patched.replace_range(open..close + "</extLst>".len(), "");
                }
            }
        }
        if patched != xml.as_str() {
            pkg.write(&path, patched)?;
            touched.insert(path);
        }
        return Ok(());
    }
    Ok(())
}

pub(super) struct SlicerRemoval<'a> {
    pub part_path: &'a str,
    pub cache_path: &'a str,
}

pub(super) fn remove_slicer<F>(
    pkg: &mut PackageEditor<'_>, workbook: &str, removal: &SlicerRemoval<'_>,
    touched: &mut HashSet<String>, mut remove_frame: F,
) -> Result<String, String>
where F: FnMut(&mut PackageEditor<'_>, &str, &str, &mut HashSet<String>) -> Result<(), String> {
    let part = if pkg.has(removal.part_path)? { Some(pkg.read_text(removal.part_path)?) } else { None };
    let cache = if pkg.has(removal.cache_path)? { Some(pkg.read_text(removal.cache_path)?) } else { None };
    let part_xml = part.as_deref().map(String::as_str).unwrap_or("");
    let cache_xml = cache.as_deref().map(String::as_str).unwrap_or("");
    let root = first(cache_xml, r#"<slicerCacheDefinition\b[^>]*>"#)?.unwrap_or("");
    let cache_name = attribute(root, "name")?;
    let controls = read_controls(part_xml)?;
    let target = cache_name.as_ref().and_then(|name| controls.iter().find(|control| &control.cache == name)).map(|control| control.name.clone());
    let remaining = target.as_ref().map_or(controls.len(), |name| controls.iter().filter(|c| &c.name != name).count());
    let tabular = first(cache_xml, r#"<tabular\b[^>]*>"#)?.unwrap_or("");
    let bound_id = attribute(tabular, "pivotCacheId")?.and_then(|id| id.parse::<u64>().ok());

    let mut hosts = Vec::new();
    for path in pkg.paths()? {
        let Some(name) = path.strip_prefix("xl/worksheets/") else { continue };
        if name.contains('/') || !name.ends_with(".xml") { continue; }
        let rels = rels_path_for(&path);
        if !pkg.has(&rels)? { continue; }
        let xml = pkg.read_text(&rels)?;
        if let Some(id) = find_relationship(&xml, "http://schemas.microsoft.com/office/2007/relationships/slicer", &path, removal.part_path)? {
            hosts.push((path, rels, id));
        }
    }
    for (host, rels, id) in hosts {
        if let Some(name) = &target { remove_frame(pkg, &host, name, touched)?; }
        if remaining == 0 {
            let relationships = pkg.read_text(&rels)?;
            pkg.write(&rels, remove_relationship(&relationships, &id)?)?;
            touched.insert(rels);
            if pkg.has(&host)? {
                let xml = pkg.read_text(&host)?;
                let patched = remove_ext_ref(&xml, &WORKSHEET_WIRING, &id)?;
                if patched != xml.as_str() { pkg.write(&host, patched)?; touched.insert(host); }
            }
        }
    }
    if let Some(name) = &target {
        if remaining > 0 {
            let (patched, _) = remove_slicer_control(part_xml, name)?;
            pkg.write(removal.part_path, patched)?;
            touched.insert(removal.part_path.to_owned());
        } else { remove_part(pkg, removal.part_path, touched)?; }
    }
    let mut still_used = false;
    if let Some(name) = &cache_name {
        for path in pkg.paths()? {
            let Some(filename) = path.strip_prefix("xl/slicers/") else { continue };
            if filename.contains('/') || !filename.ends_with(".xml") { continue; }
            if read_controls(&pkg.read_text(&path)?)?.iter().any(|control| &control.cache == name) {
                still_used = true; break;
            }
        }
    }
    let mut workbook = workbook.to_owned();
    if !still_used {
        if let Some(name) = &cache_name { workbook = remove_slicer_defined_name(&workbook, name)?; }
        let rels = "xl/_rels/workbook.xml.rels";
        if pkg.has(rels)? {
            let xml = pkg.read_text(rels)?;
            if let Some(id) = find_relationship(&xml, "http://schemas.microsoft.com/office/2007/relationships/slicerCache", "xl/workbook.xml", removal.cache_path)? {
                pkg.write(rels, remove_relationship(&xml, &id)?)?;
                touched.insert(rels.to_owned());
                workbook = remove_ext_ref(&workbook, &WORKBOOK_WIRING, &id)?;
            }
        }
        remove_part(pkg, removal.cache_path, touched)?;
        if let Some(id) = bound_id { remove_pivot_cache_slicer_ext(pkg, &workbook, id, touched)?; }
    }
    Ok(workbook)
}

pub(super) fn apply_slicer_state<Add, Remove>(
    pkg: &mut PackageEditor<'_>, workbook: &str, controls: &[SlicerControl<'_>],
    removals: &[SlicerRemoval<'_>], touched: &mut HashSet<String>, mut add_frame: Add, mut remove_frame: Remove,
) -> Result<String, String>
where
    Add: FnMut(&mut PackageEditor<'_>, &str, &str, &mut HashSet<String>) -> Result<(), String>,
    Remove: FnMut(&mut PackageEditor<'_>, &str, &str, &mut HashSet<String>) -> Result<(), String>,
{
    let written: HashSet<&str> = controls.iter().filter_map(|control| control.cache_path).collect();
    let mut workbook = workbook.to_owned();
    for removal in removals {
        if written.contains(removal.cache_path) {
            return Err("A removed slicer cache cannot also be written in the same save.".to_owned());
        }
        workbook = remove_slicer(pkg, &workbook, removal, touched, &mut remove_frame)?;
    }
    if !controls.is_empty() {
        let (mut names, mut caches) = collect_slicer_names(pkg)?;
        for control in controls {
            workbook = write_slicer(pkg, &workbook, control, touched, &mut names, &mut caches, &mut add_frame)?;
        }
    }
    Ok(workbook)
}

pub(super) fn apply_slicers(
    pkg: &mut PackageEditor<'_>, workbook: &str, controls: &[SlicerControl<'_>],
    removals: &[SlicerRemoval<'_>], touched: &mut HashSet<String>,
) -> Result<String, String> {
    apply_slicer_state(pkg, workbook, controls, removals, touched, ensure_control_frame, remove_control_frame)
}

fn input_string<'a>(value: &'a serde_json::Value, name: &str) -> Result<&'a str, String> {
    value.get(name).and_then(serde_json::Value::as_str).ok_or_else(|| format!("Slicer state is missing {name}."))
}

pub(super) fn apply_slicer_phase(
    pkg: &mut PackageEditor<'_>, state: &mut super::state::PlannerState,
    resolved: &serde_json::Value, touched: &mut HashSet<String>,
) -> Result<(), String> {
    let Some(value) = resolved.get("slicerState").filter(|value| !value.is_null()) else { return Ok(()) };
    let input = value.get("slicers").and_then(serde_json::Value::as_array).ok_or("Slicer state is missing slicers.")?;
    let removed = value.get("removals").and_then(serde_json::Value::as_array).ok_or("Slicer state is missing removals.")?;
    let mut paths = Vec::with_capacity(input.len());
    let mut all_items = Vec::with_capacity(input.len());
    for control in input {
        paths.push(state.path_for(pkg, input_string(control, "sheetName")?)?);
        let items = control.get("items").and_then(serde_json::Value::as_array).ok_or("Slicer state is missing items.")?;
        let items = items.iter().map(|item| Ok(SlicerItem {
            x: item.get("x").and_then(serde_json::Value::as_number).cloned().ok_or("Invalid slicer item index.")?,
            selected: item.get("selected").and_then(serde_json::Value::as_bool).ok_or("Invalid slicer item selection.")?,
        })).collect::<Result<Vec<_>, String>>()?;
        all_items.push(items);
    }
    let mut controls = Vec::with_capacity(input.len());
    for ((control, path), items) in input.iter().zip(&paths).zip(&all_items) {
        controls.push(SlicerControl {
            worksheet_path: path, pivot_path: input_string(control, "pivotPath")?,
            source_name: input_string(control, "sourceName")?, caption: input_string(control, "caption")?,
            name: control.get("name").and_then(serde_json::Value::as_str),
            part_path: control.get("partPath").and_then(serde_json::Value::as_str),
            cache_path: control.get("cachePath").and_then(serde_json::Value::as_str),
            cache_name: control.get("cacheName").and_then(serde_json::Value::as_str), items,
        });
    }
    let removals = removed.iter().map(|removal| Ok(SlicerRemoval {
        part_path: input_string(removal, "partPath")?, cache_path: input_string(removal, "cachePath")?,
    })).collect::<Result<Vec<_>, String>>()?;
    if controls.is_empty() && removals.is_empty() { return Ok(()); }
    state.workbook_xml = std::sync::Arc::new(apply_slicers(pkg, &state.workbook_xml, &controls, &removals, touched)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{add_slicer_defined_name, build_slicer_cache_xml, remove_slicer_control, upsert_slicer_control, SlicerItem};
    use std::{collections::HashSet, sync::Arc};
    use super::super::package::{EntrySource, PackageEditor};

    struct Source(Vec<(String, String)>);
    impl EntrySource for Source {
        fn paths(&mut self) -> Result<Vec<String>, String> { Ok(self.0.iter().map(|(path, _)| path.clone()).collect()) }
        fn read_text(&mut self, path: &str) -> Result<Arc<String>, String> {
            self.0.iter().find(|(name, _)| name == path).map(|(_, xml)| Arc::new(xml.clone())).ok_or_else(|| format!("missing {path}"))
        }
        fn can_patch(&mut self, _: &str) -> Result<bool, String> { Ok(true) }
        fn contains_text(&mut self, path: &str, needle: &str) -> Result<bool, String> { Ok(self.read_text(path)?.contains(needle)) }
        fn release_text(&mut self, _: &str) {}
        fn short_date_pattern(&mut self) -> Result<String, String> { Ok("m/d/yyyy".into()) }
    }

    #[test]
    fn slicer_creation_wires_cache_worksheet_and_visible_frame() {
        let workbook = r#"<workbook><sheets><sheet name="Sheet1" sheetId="3" r:id="rId1"/></sheets><pivotCaches><pivotCache cacheId="9" r:id="rId2"/></pivotCaches></workbook>"#;
        let mut source = Source(vec![
            ("[Content_Types].xml".into(), "<Types></Types>".into()),
            ("xl/_rels/workbook.xml.rels".into(), r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="pivotCache/pivotCacheDefinition1.xml"/></Relationships>"#.into()),
            ("xl/worksheets/sheet1.xml".into(), "<worksheet><sheetData/></worksheet>".into()),
            ("xl/worksheets/_rels/sheet1.xml.rels".into(), r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/pivotTable" Target="../pivotTables/pivotTable1.xml"/></Relationships>"#.into()),
            ("xl/pivotTables/pivotTable1.xml".into(), r#"<pivotTableDefinition name="Sales" cacheId="9"/>"#.into()),
            ("xl/pivotCache/pivotCacheDefinition1.xml".into(), "<pivotCacheDefinition/>".into()),
        ]);
        let mut pkg = PackageEditor::new(&mut source);
        let mut touched = HashSet::new();
        let items = [SlicerItem { x: 1.into(), selected: true }];
        let control = super::SlicerControl { worksheet_path: "xl/worksheets/sheet1.xml", pivot_path: "xl/pivotTables/pivotTable1.xml", source_name: "City", caption: "City", name: None, part_path: None, cache_path: None, cache_name: None, items: &items };
        let workbook = super::apply_slicers(&mut pkg, workbook, &[control], &[], &mut touched).unwrap();
        assert!(workbook.contains(r#"<definedName name="Slicer_City">#N/A</definedName>"#));
        assert!(workbook.contains("<x14:slicerCache "));
        let cache = pkg.read_text("xl/slicerCaches/slicerCache1.xml").unwrap();
        assert!(cache.contains(r#"<pivotTable tabId="3" name="Sales"/>"#));
        assert!(cache.contains(r#"<tabular pivotCacheId="1" crossFilter="showItemsWithDataAtTop">"#));
        let sheet = pkg.read_text("xl/worksheets/sheet1.xml").unwrap();
        assert!(sheet.contains("<drawing "));
        assert!(sheet.contains("<x14:slicer "));
        let drawing = pkg.read_text("xl/drawings/drawing1.xml").unwrap();
        assert!(drawing.contains(r#"<sle:slicer xmlns:sle="http://schemas.microsoft.com/office/drawing/2010/slicer" name="City"/>"#));
        let removal = super::SlicerRemoval { part_path: "xl/slicers/slicer1.xml", cache_path: "xl/slicerCaches/slicerCache1.xml" };
        let workbook = super::apply_slicers(&mut pkg, &workbook, &[], &[removal], &mut touched).unwrap();
        assert!(!workbook.contains("Slicer_City"));
        assert!(!pkg.has("xl/slicers/slicer1.xml").unwrap());
        assert!(!pkg.has("xl/slicerCaches/slicerCache1.xml").unwrap());
        assert!(!pkg.read_text("xl/drawings/drawing1.xml").unwrap().contains("<sle:slicer "));
        assert!(!pkg.read_text("xl/pivotCache/pivotCacheDefinition1.xml").unwrap().contains("pivotCacheId="));
    }

    #[test]
    fn cache_binding_allocates_distinct_x14_id_and_preserves_nested_extension() {
        let workbook = r#"<workbook><pivotCaches><pivotCache cacheId="10" r:id="rId1"/><pivotCache cacheId="20" r:id="rId2"/></pivotCaches></workbook>"#;
        let mut source = Source(vec![
            ("xl/_rels/workbook.xml.rels".into(), r#"<Relationships><Relationship Id="rId1" Target="pivotCache/pivotCacheDefinition1.xml"/><Relationship Id="rId2" Target="pivotCache/pivotCacheDefinition2.xml"/></Relationships>"#.into()),
            ("xl/pivotCache/pivotCacheDefinition1.xml".into(), format!(r#"<pivotCacheDefinition><extLst><ext uri="{}"><x14:pivotCacheDefinition pivotCacheId="41"/></ext></extLst></pivotCacheDefinition>"#, super::CACHE_EXT_URI)),
            ("xl/pivotCache/pivotCacheDefinition2.xml".into(), r#"<pivotCacheDefinition><cacheFields><cacheField><extLst><ext uri="nested"/></extLst></cacheField></cacheFields></pivotCacheDefinition>"#.into()),
        ]);
        let mut pkg = PackageEditor::new(&mut source);
        let mut touched = HashSet::new();
        assert_eq!(super::ensure_pivot_cache_slicer_ext(&mut pkg, workbook, 20, &mut touched).unwrap(), 42);
        let xml = pkg.read_text("xl/pivotCache/pivotCacheDefinition2.xml").unwrap();
        assert!(xml.contains(r#"<cacheField><extLst><ext uri="nested"/></extLst></cacheField>"#));
        assert!(xml.contains(r#"<x14:pivotCacheDefinition pivotCacheId="42"/>"#));
    }

    #[test]
    fn cache_rewrite_preserves_connections_extensions_and_excel_attributes() {
        let old = r#"<slicerCacheDefinition xmlns="old" name="old" xr10:uid="{A}"><pivotTables><pivotTable tabId="8" name="First"/><pivotTable tabId="9" name="Second"/></pivotTables><data><tabular pivotCacheId="7" sortOrder="ascending" crossFilter="none"><items count="0"/></tabular></data><extLst><ext uri="custom"/></extLst></slicerCacheDefinition>"#;
        let xml = build_slicer_cache_xml("Cache & One", "City", 3, "Pivot", 42, &[SlicerItem { x: 2.into(), selected: true }, SlicerItem { x: 5.into(), selected: false }], Some(old)).unwrap();
        assert!(xml.contains(r#"xr10:uid="{A}" name="Cache &amp; One" sourceName="City""#));
        assert!(xml.contains(r#"<pivotTable tabId="9" name="Second"/>"#));
        assert!(xml.contains(r#"<tabular pivotCacheId="42" sortOrder="ascending" crossFilter="none"><items count="2"><i x="2" s="1"/><i x="5"/></items>"#));
        assert!(xml.contains(r#"<extLst><ext uri="custom"/></extLst>"#));
    }

    #[test]
    fn changing_one_control_keeps_other_control_and_children() {
        let old = r#"<slicers xmlns="old" xmlns:foo="urn:foo"><slicer name="One" cache="C1" caption="Old" style="Dark"><extLst><ext uri="x"/></extLst></slicer><slicer name="Two" cache="C2" caption="Other" rowHeight="5"/></slicers>"#;
        let updated = upsert_slicer_control(Some(old), "One", "C1", "New &amp;").unwrap();
        assert!(updated.contains(r#"<slicer name="One" cache="C1" caption="New &amp;amp;" style="Dark" rowHeight="241300"><extLst><ext uri="x"/></extLst></slicer>"#));
        assert!(updated.contains(r#"<slicer name="Two" cache="C2" caption="Other" rowHeight="5"/>"#));
        let (removed, empty) = remove_slicer_control(&updated, "One").unwrap();
        assert!(!empty);
        assert!(!removed.contains(r#"name="One""#));
        assert!(removed.contains(r#"name="Two""#));
    }

    #[test]
    fn defined_name_uses_cache_identity_and_schema_order() {
        let xml = r#"<workbook><sheets></sheets><calcPr/><extLst/></workbook>"#;
        assert_eq!(add_slicer_defined_name(xml, "Slicer_City").unwrap(), r#"<workbook><sheets></sheets><definedNames><definedName name="Slicer_City">#N/A</definedName></definedNames><calcPr/><extLst/></workbook>"#);
        let xml = r#"<workbook><sheets></sheets><definedNames/></workbook>"#;
        assert_eq!(add_slicer_defined_name(xml, "C").unwrap(), r#"<workbook><sheets></sheets><definedNames><definedName name="C">#N/A</definedName></definedNames></workbook>"#);
    }

    #[test]
    fn slicer_reference_does_not_enter_nested_extension_list() {
        let xml = r#"<worksheet><sheetData><row><c><extLst><ext uri="cell"/></extLst></c></row></sheetData><extLst><ext uri="other"/></extLst></worksheet>"#;
        let patched = super::add_ext_ref(xml, "worksheet", &super::WORKSHEET_WIRING, "rId4").unwrap();
        assert!(patched.contains(r#"<c><extLst><ext uri="cell"/></extLst></c>"#));
        assert!(patched.contains(r#"<x14:slicer r:id="rId4"/>"#));
        assert_eq!(super::add_ext_ref(&patched, "worksheet", &super::WORKSHEET_WIRING, "rId4").unwrap(), patched);
    }
}
