use std::collections::HashSet;
use base64::Engine;
use jscompat::TryJsRegex;
use serde_json::Value;
use super::{first_span, register_default, substitute};
use super::super::{package::PackageEditor, pivots::{allocate_part_path, append_relationship, register_content_type_override, relative_target, rels_path_for}, state::PlannerState};

const HEADER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
const RICH: &str = "http://schemas.microsoft.com/office/spreadsheetml/2017/richdata";
const OFFICE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const METADATA: &str = "xl/metadata.xml";
const STRUCTURE: &str = "xl/richData/rdrichvaluestructure.xml";
const VALUES: &str = "xl/richData/rdrichvalue.xml";
const RICH_RELS: &str = "xl/richData/richValueRel.xml";
const META_TYPE: &str = "<metadataType name=\"XLRICHVALUE\" minSupportedVersion=\"120000\" copy=\"1\" pasteAll=\"1\" pasteValues=\"1\" merge=\"1\" splitFirst=\"1\" rowColShift=\"1\" clearFormats=\"1\" clearComments=\"1\" assign=\"1\" coerce=\"1\"/>";
const STRUCTURE_XML: &str = "<s t=\"_localImage\"><k n=\"_rvRel:LocalImageIdentifier\" t=\"i\"/><k n=\"CalcOrigin\" t=\"i\"/></s>";

fn count(xml: &str, pattern: &str) -> Result<usize, String> {
    let re = TryJsRegex::new(pattern, "g").map_err(|e| e.to_string())?;
    let mut n = 0; for m in re.try_find_iter(xml) { m.map_err(|e| e.to_string())?; n += 1; } Ok(n)
}
fn count_part(pkg: &mut PackageEditor<'_>, path: &str, pattern: &str) -> Result<usize, String> {
    if pkg.has(path)? { count(&pkg.read_text(path)?, pattern) } else { Ok(0) }
}
fn bump_tag(tag: &str, delta: usize) -> Result<String, String> {
    let re = TryJsRegex::new(r#"\bcount="(\d+)""#, "").map_err(|e| e.to_string())?;
    let hit = re.try_find_iter(tag).next().transpose().map_err(|e| e.to_string())?;
    if let Some(hit) = hit { let n = hit.get(1).and_then(|v| v.parse::<usize>().ok()).ok_or("Invalid count.")?; Ok(substitute(tag, hit.start(), hit.end(), &format!("count=\"{}\"", n + delta))) }
    else { Ok(substitute(tag, tag.len()-1, tag.len()-1, &format!(" count=\"{delta}\""))) }
}
fn append_counted(xml: &str, tag: &str, records: &str, delta: usize, message: &str) -> Result<String, String> {
    let pattern = format!(r"<{tag}\b[^>]*>");
    let (a,b) = first_span(xml, &pattern)?.ok_or(message.to_owned())?;
    let open = &xml[a..b]; let bumped = bump_tag(open, delta)?;
    let mut out = substitute(xml, a,b,&bumped);
    let close = format!("</{tag}>");
    let at = out.find(&close).ok_or(message.to_owned())?;
    out.insert_str(at, records); Ok(out)
}
fn value_bk(index: usize) -> String {
    format!("<bk><extLst><ext uri=\"{{3e2802c4-a4d2-4d8b-9148-e3be6c30e623}}\"><xlrd:rvb i=\"{index}\"/></ext></extLst></bk>")
}
fn metadata_new(indexes: &[usize]) -> String {
    let future = indexes.iter().map(|i| value_bk(*i)).collect::<String>();
    let values = indexes.iter().enumerate().map(|(i,_)| format!("<bk><rc t=\"1\" v=\"{i}\"/></bk>")).collect::<String>();
    format!("{HEADER}<metadata xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:xlrd=\"{RICH}\"><metadataTypes count=\"1\">{META_TYPE}</metadataTypes><futureMetadata name=\"XLRICHVALUE\" count=\"{}\">{future}</futureMetadata><valueMetadata count=\"{}\">{values}</valueMetadata></metadata>", indexes.len(), indexes.len())
}
fn metadata_append(xml: &str, indexes: &[usize]) -> Result<(String, Vec<usize>), String> {
    let mut xml = xml.to_owned();
    let (a,b) = first_span(&xml, r"<metadata\b[^>]*>")?.ok_or("xl/metadata.xml is malformed.")?;
    if !xml[a..b].contains("xmlns:xlrd=") { xml = substitute(&xml, b-1,b-1,&format!(" xmlns:xlrd=\"{RICH}\"")); }
    let (a,b) = first_span(&xml, r"<metadataTypes\b[^>]*>[\s\S]*?</metadataTypes>")?.ok_or("xl/metadata.xml is malformed.")?;
    let block = &xml[a..b];
    let re = TryJsRegex::new(r#"<metadataType\b[^>]*\bname="([^"]*)""#, "g").map_err(|e| e.to_string())?;
    let mut names = Vec::new();
    for m in re.try_find_iter(block) { names.push(m.map_err(|e| e.to_string())?.get(1).unwrap_or("").to_owned()); }
    let type_index = if let Some(i) = names.iter().position(|s| s == "XLRICHVALUE") { i+1 } else {
        let updated = append_counted(block, "metadataTypes", META_TYPE, 1, "xl/metadata.xml is malformed.")?;
        xml = substitute(&xml,a,b,&updated); names.len()+1
    };
    let records = indexes.iter().map(|i| value_bk(*i)).collect::<String>();
    let future = first_span(&xml, r#"<futureMetadata\b[^>]*\bname="XLRICHVALUE"[^>]*>[\s\S]*?</futureMetadata>"#)?;
    let future_base = if let Some((a,b)) = future {
        let block = &xml[a..b]; let n = count(block, r"<bk\b")?;
        let updated = append_counted(block, "futureMetadata", &records, indexes.len(), "xl/metadata.xml is malformed.")?;
        xml = substitute(&xml,a,b,&updated); n
    } else {
        let element = format!("<futureMetadata name=\"XLRICHVALUE\" count=\"{}\">{records}</futureMetadata>", indexes.len());
        let at = ["<cellMetadata", "<valueMetadata", "</metadata>"].iter().filter_map(|m| xml.find(m)).min().ok_or("xl/metadata.xml is malformed.")?;
        xml = substitute(&xml,at,at,&element); 0
    };
    let records = indexes.iter().enumerate().map(|(i,_)| format!("<bk><rc t=\"{type_index}\" v=\"{}\"/></bk>", future_base+i)).collect::<String>();
    let vals = first_span(&xml, r"<valueMetadata\b[^>]*>[\s\S]*?</valueMetadata>")?;
    let base = if let Some((a,b)) = vals {
        let block = &xml[a..b]; let n = count(block, r"<bk\b")?;
        let updated = append_counted(block, "valueMetadata", &records, indexes.len(), "xl/metadata.xml is malformed.")?;
        xml = substitute(&xml,a,b,&updated); n
    } else {
        let at = xml.find("</metadata>").ok_or("xl/metadata.xml is malformed.")?;
        xml = substitute(&xml,at,at,&format!("<valueMetadata count=\"{}\">{records}</valueMetadata>", indexes.len())); 0
    };
    Ok((xml, (1..=indexes.len()).map(|i| base+i).collect()))
}

fn cell_address(row: u64, mut col: u64) -> String {
    let mut letters = String::new();
    loop {
        letters.insert(0, (b'A' + (col % 26) as u8) as char);
        if col < 26 { break; }
        col = col / 26 - 1;
    }
    format!("{letters}{}", row + 1)
}
fn column(address: &str) -> u64 {
    address.bytes().take_while(u8::is_ascii_uppercase).fold(0u64, |n, b| n * 26 + u64::from(b - b'A' + 1)).saturating_sub(1)
}
fn patch_cell(xml: &str, row: u64, col: u64, vm: usize) -> Result<String, String> {
    let address = cell_address(row, col);
    let pattern = format!(r#"<c\b([^>]*?)\br="{address}"([^>]*?)(?:/>|>([\s\S]*?)</c>)"#);
    let re = TryJsRegex::new(&pattern, "").map_err(|e| e.to_string())?;
    let found = re.try_find_iter(xml).next().transpose().map_err(|e| e.to_string())?;
    let style = if let Some(m) = &found {
        let attrs = format!("{} {}", m.get(1).unwrap_or(""), m.get(2).unwrap_or(""));
        let attr = TryJsRegex::new(r#"\bs="([^"]*)""#, "").map_err(|e| e.to_string())?;
        attr.try_find_iter(&attrs).next().transpose().map_err(|e| e.to_string())?.and_then(|m| m.get(1).map(str::to_owned))
    } else { None };
    let style = style.map(|s| format!(" s=\"{s}\"")).unwrap_or_default();
    let cell = format!("<c r=\"{address}\"{style} t=\"e\" vm=\"{vm}\"><v>#VALUE!</v></c>");
    if let Some(m) = found { return Ok(substitute(xml, m.start(), m.end(), &cell)); }
    let row_number = row + 1;
    let row_pattern = format!(r#"<row\b([^>]*?\br="{row_number}"[^>]*?)(/>|>([\s\S]*?)</row>)"#);
    let re = TryJsRegex::new(&row_pattern, "").map_err(|e| e.to_string())?;
    if let Some(m) = re.try_find_iter(xml).next().transpose().map_err(|e| e.to_string())? {
        let attrs = m.get(1).unwrap_or("");
        let body = m.get(3).unwrap_or("");
        let siblings = TryJsRegex::new(r#"<c\b[^>]*?\br="([A-Z]{1,3})[1-9][0-9]*""#, "g").map_err(|e| e.to_string())?;
        let mut at = body.len();
        for sibling in siblings.try_find_iter(body) { let sibling = sibling.map_err(|e| e.to_string())?; if column(sibling.get(1).unwrap_or("")) > col { at = sibling.start(); break; } }
        let body = substitute(body, at,at,&cell);
        return Ok(substitute(xml,m.start(),m.end(),&format!("<row{attrs}>{body}</row>")));
    }
    let new_row = format!("<row r=\"{row_number}\">{cell}</row>");
    let rows = TryJsRegex::new(r#"<row\b[^>]*?\br="([1-9][0-9]*)""#, "g").map_err(|e| e.to_string())?;
    for m in rows.try_find_iter(xml) {
        let m = m.map_err(|e| e.to_string())?;
        if m.get(1).and_then(|s| s.parse::<u64>().ok()).is_some_and(|n| n > row_number) { return Ok(substitute(xml,m.start(),m.start(),&new_row)); }
    }
    if let Some(at) = xml.find("</sheetData>") { return Ok(substitute(xml,at,at,&new_row)); }
    if let Some((a,b)) = first_span(xml, r"<sheetData\s*/>")? { return Ok(substitute(xml,a,b,&format!("<sheetData>{new_row}</sheetData>"))); }
    Err("Worksheet has no sheetData element.".into())
}

pub(super) fn apply(pkg: &mut PackageEditor<'_>, state: &mut PlannerState, additions: &[Value], touched: &mut HashSet<String>) -> Result<(), String> {
    if additions.is_empty() { return Ok(()); }
    let mut paths = Vec::new();
    for image in additions {
        let mime = image.get("mediaType").and_then(Value::as_str).ok_or("Cell image has no media type.")?;
        let ext = match mime { "image/png" => "png", "image/jpeg" => "jpeg", "image/gif" => "gif", _ => return Err(format!("Unsupported image media type {mime}.")) };
        let path = allocate_part_path(pkg, "xl/media/image", &format!(".{ext}"))?;
        let encoded = image.get("base64").and_then(Value::as_str).ok_or("Cell image has no bytes.")?;
        let encoded: String = encoded.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/' || *c == '-' || *c == '_').map(|c| match c { '-' => '+', '_' => '/', _ => c }).collect();
        let bytes = base64::engine::general_purpose::STANDARD_NO_PAD.decode(encoded).map_err(|e| e.to_string())?;
        pkg.add_binary(&path, bytes)?; touched.insert(path.clone()); paths.push(path);
        register_default(pkg, ext, mime, touched)?;
    }
    let value_base = count_part(pkg, VALUES, r"<rv\b")?;
    let rel_base = count_part(pkg, RICH_RELS, r"<rel\b")?;
    let structure_index = if pkg.has(STRUCTURE)? {
        let xml = pkg.read_text(STRUCTURE)?;
        let re = TryJsRegex::new(r"<s\b[^>]*>", "g").map_err(|e| e.to_string())?;
        let mut structures = Vec::new();
        for m in re.try_find_iter(&xml) { structures.push(m.map_err(|e| e.to_string())?.as_str().to_owned()); }
        if let Some(i) = structures.iter().position(|s| s.contains("t=\"_localImage\"")) { i } else {
            let next = append_counted(&xml, "rvStructures", STRUCTURE_XML, 1, "rdrichvaluestructure.xml is malformed.")?;
            pkg.write(STRUCTURE, next)?; touched.insert(STRUCTURE.into()); structures.len()
        }
    } else {
        pkg.add(STRUCTURE, format!("{HEADER}<rvStructures xmlns=\"{RICH}\" count=\"1\">{STRUCTURE_XML}</rvStructures>")); touched.insert(STRUCTURE.into()); 0
    };
    let indexes: Vec<_> = (0..additions.len()).map(|i| value_base + i).collect();
    let values = indexes.iter().enumerate().map(|(i,_)| format!("<rv s=\"{structure_index}\"><v>{}</v><v>5</v></rv>", rel_base+i)).collect::<String>();
    if pkg.has(VALUES)? { let xml = pkg.read_text(VALUES)?; pkg.write(VALUES, append_counted(&xml, "rvData", &values, additions.len(), "rdrichvalue.xml is malformed.")?)?; }
    else { pkg.add(VALUES, format!("{HEADER}<rvData xmlns=\"{RICH}\" count=\"{}\">{values}</rvData>", additions.len())); }
    touched.insert(VALUES.into());
    let rels = rels_path_for(RICH_RELS);
    let mut elements = String::new();
    for path in &paths {
        let id = append_relationship(pkg, &rels, &format!("{OFFICE}/image"), &relative_target(RICH_RELS, path), false)?;
        elements.push_str(&format!("<rel r:id=\"{id}\"/>"));
    }
    touched.insert(rels);
    if pkg.has(RICH_RELS)? {
        let xml = pkg.read_text(RICH_RELS)?;
        let at = xml.find("</richValueRels>").ok_or("richValueRel.xml is malformed.")?;
        pkg.write(RICH_RELS, substitute(&xml,at,at,&elements))?;
    } else { pkg.add(RICH_RELS, format!("{HEADER}<richValueRels xmlns=\"http://schemas.microsoft.com/office/spreadsheetml/2022/richvaluerel\" xmlns:r=\"{OFFICE}\">{elements}</richValueRels>")); }
    touched.insert(RICH_RELS.into());
    let vm = if pkg.has(METADATA)? {
        let xml = pkg.read_text(METADATA)?; let (next, vm) = metadata_append(&xml, &indexes)?; pkg.write(METADATA, next)?; vm
    } else { pkg.add(METADATA, metadata_new(&indexes)); (1..=indexes.len()).collect() };
    touched.insert(METADATA.into());
    let workbook_rels = "xl/_rels/workbook.xml.rels";
    let original_rels = pkg.read_text(workbook_rels)?;
    let wanted = [
        ("http://schemas.openxmlformats.org/officeDocument/2006/relationships/sheetMetadata", "metadata.xml", METADATA, "application/vnd.openxmlformats-officedocument.spreadsheetml.sheetMetadata+xml"),
        ("http://schemas.microsoft.com/office/2022/10/relationships/richValueRel", "richData/richValueRel.xml", RICH_RELS, "application/vnd.ms-excel.richvaluerel+xml"),
        ("http://schemas.microsoft.com/office/2017/06/relationships/rdRichValue", "richData/rdrichvalue.xml", VALUES, "application/vnd.ms-excel.rdrichvalue+xml"),
        ("http://schemas.microsoft.com/office/2017/06/relationships/rdRichValueStructure", "richData/rdrichvaluestructure.xml", STRUCTURE, "application/vnd.ms-excel.rdrichvaluestructure+xml"),
    ];
    for (kind,target,path,content) in wanted {
        if !original_rels.contains(&format!("Type=\"{kind}\"")) { append_relationship(pkg, workbook_rels, kind, target, false)?; touched.insert(workbook_rels.into()); }
        register_content_type_override(pkg, path, content, touched)?;
    }
    for (index, image) in additions.iter().enumerate() {
        let sheet = image.get("sheetName").and_then(Value::as_str).ok_or("Cell image has no sheet name.")?;
        let path = state.path_for(pkg, sheet)?;
        let row = image.get("row").and_then(Value::as_u64).ok_or("Cell image has no row.")?;
        let col = image.get("column").and_then(Value::as_u64).ok_or("Cell image has no column.")?;
        let xml = pkg.read_text(&path)?;
        pkg.write(&path, patch_cell(&xml, row, col, vm[index])?)?; touched.insert(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_extension_preserves_existing_indexes() {
        let old = metadata_new(&[0, 1]);
        let (updated, vm) = metadata_append(&old, &[2, 3]).unwrap();
        assert_eq!(vm, vec![3, 4]);
        assert!(updated.contains("<futureMetadata name=\"XLRICHVALUE\" count=\"4\">"));
        assert!(updated.contains("<bk><rc t=\"1\" v=\"2\"/></bk>"));
        assert!(updated.contains("<xlrd:rvb i=\"3\"/>"));
    }
    #[test]
    fn cell_image_preserves_style_and_inserts_in_order() {
        let sheet = "<worksheet><sheetData><row r=\"2\"><c r=\"B2\" s=\"4\"><v>9</v></c><c r=\"D2\"><v>1</v></c></row></sheetData></worksheet>";
        let sheet = patch_cell(sheet, 1, 1, 7).unwrap();
        assert!(sheet.contains("<c r=\"B2\" s=\"4\" t=\"e\" vm=\"7\"><v>#VALUE!</v></c>"));
        let sheet = patch_cell(&sheet, 1, 2, 8).unwrap();
        assert!(sheet.find("r=\"C2\"").unwrap() < sheet.find("r=\"D2\"").unwrap());
        let sheet = patch_cell(&sheet, 0, 26, 9).unwrap();
        assert!(sheet.find("r=\"AA1\"").unwrap() < sheet.find("r=\"B2\"").unwrap());
    }
}
