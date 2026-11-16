mod shift;
pub(super) use shift::{InsertedTableColumn, TableColumnInsertion, shift_table_part};

use super::{
    package::PackageEditor,
    state::{PlannerState, resolve_rel_target},
    xml,
};
use serde_json::Value;
use std::collections::HashSet;

const TABLE_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/table";
const CONTENT_TYPE: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.table+xml";
const CONTENT: &str = "[Content_Types].xml";

#[derive(Clone, Copy, Debug)]
pub(super) struct Area {
    pub r1: i64,
    pub c1: i64,
    pub r2: i64,
    pub c2: i64,
}
impl Area {
    pub fn from_value(v: &Value) -> Result<Self, String> {
        let n = |k| {
            v.get(k)
                .and_then(Value::as_i64)
                .ok_or_else(|| format!("Table range needs {k}."))
        };
        Ok(Self {
            r1: n("startRow")?,
            c1: n("startColumn")?,
            r2: n("endRow")?,
            c2: n("endColumn")?,
        })
    }
    pub fn reference(self) -> String {
        format!(
            "{}{}:{}{}",
            column(self.c1),
            self.r1 + 1,
            column(self.c2),
            self.r2 + 1
        )
    }
    pub fn overlaps(self, other: Self) -> bool {
        self.r1 <= other.r2 && other.r1 <= self.r2 && self.c1 <= other.c2 && other.c1 <= self.c2
    }
    pub fn contains(self, other: Self) -> bool {
        self.r1 <= other.r1 && self.r2 >= other.r2 && self.c1 <= other.c1 && self.c2 >= other.c2
    }
}
pub(super) fn column(mut n: i64) -> String {
    let mut s = String::new();
    n += 1;
    while n > 0 {
        n -= 1;
        s.insert(0, (b'A' + (n % 26) as u8) as char);
        n /= 26;
    }
    s
}
pub(super) fn parse_ref(s: &str) -> Result<Area, String> {
    fn cell(s: &str) -> Option<(i64, i64)> {
        let i = s.find(|c: char| c.is_ascii_digit())?;
        let (letters, digits) = s.split_at(i);
        if letters.is_empty()
            || !letters.bytes().all(|b| b.is_ascii_uppercase())
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let mut c = 0i64;
        for b in letters.bytes() {
            c = c.checked_mul(26)?.checked_add((b - b'A' + 1) as i64)?;
        }
        Some((digits.parse::<i64>().ok()?.checked_sub(1)?, c - 1))
    }
    let clean = s.replace('$', "");
    let mut parts = clean.split(':');
    let a = parts.next().and_then(cell);
    let b = parts.next().map(cell).unwrap_or(a);
    match (a, b, parts.next()) {
        (Some((r1, c1)), Some((r2, c2)), None) => Ok(Area { r1, c1, r2, c2 }),
        _ => Err(format!(
            "Unsupported range reference \"{s}\" in the workbook."
        )),
    }
}
pub(super) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
pub(super) fn rels_path(path: &str) -> Result<String, String> {
    let (dir, file) = path.rsplit_once('/').ok_or("Invalid worksheet path.")?;
    Ok(format!("{dir}/_rels/{file}.rels"))
}
pub(super) fn first(xml_text: &str, name: &str) -> Result<Option<xml::Element>, String> {
    Ok(xml::root_elements(xml_text, name)?.into_iter().next())
}
pub(super) fn replace_range(s: &str, start: usize, end: usize, replacement: &str) -> String {
    format!("{}{}{}", &s[..start], replacement, &s[end..])
}
fn root_tail(text: &str, name: &str) -> Result<usize, String> {
    let root = first(text, name)?.ok_or_else(|| format!("The {name} part is malformed."))?;
    let tail = xml::root_elements(text, "extLst")?
        .into_iter()
        .find(|e| e.end <= root.close_start && text[e.end..root.close_start].trim().is_empty());
    Ok(tail.map_or(root.close_start, |e| e.start))
}
fn attributes(tag: &xml::Element) -> Vec<(String, String)> {
    tag.attributes.clone()
}
fn set(attrs: &mut Vec<(String, String)>, key: &str, value: Option<String>) {
    if let Some(pos) = attrs.iter().position(|(k, _)| k == key) {
        if let Some(value) = value {
            attrs[pos].1 = value;
        } else {
            attrs.remove(pos);
        }
    } else if let Some(value) = value {
        attrs.push((key.into(), value));
    }
}
fn open_tag(name: &str, attrs: &[(String, String)], closed: bool) -> String {
    let attrs = attrs
        .iter()
        .map(|(k, v)| format!(" {k}=\"{}\"", escape(v)))
        .collect::<String>();
    format!("<{name}{attrs}{}>", if closed { "/" } else { "" })
}
#[derive(Clone)]
pub(super) struct LocatedTable {
    pub path: String,
    pub rel_id: String,
    pub xml: String,
}
pub(super) fn sheet_tables(
    pkg: &mut PackageEditor<'_>,
    worksheet: &str,
) -> Result<Vec<LocatedTable>, String> {
    let path = rels_path(worksheet)?;
    if !pkg.has(&path)? {
        return Ok(Vec::new());
    }
    let rels = pkg.read_text(&path)?;
    let mut out = Vec::new();
    for rel in xml::root_elements(&rels, "Relationship")? {
        if rel.attribute("Type") != Some(TABLE_REL) {
            continue;
        }
        let (Some(id), Some(target)) = (rel.attribute("Id"), rel.attribute("Target")) else {
            continue;
        };
        let path = resolve_rel_target(worksheet, target)?;
        if pkg.has(&path)? {
            let text = pkg.read_text(&path)?;
            out.push(LocatedTable {
                path,
                rel_id: id.to_owned(),
                xml: text.as_ref().clone(),
            });
        }
    }
    Ok(out)
}
fn tables(pkg: &mut PackageEditor<'_>) -> Result<Vec<String>, String> {
    Ok(pkg
        .paths()?
        .into_iter()
        .filter(|p| p.starts_with("xl/tables/") && !p[10..].contains('/') && p.ends_with(".xml"))
        .collect())
}
fn names(xml_text: &str) -> Result<Vec<String>, String> {
    let Some(t) = first(xml_text, "table")? else {
        return Ok(Vec::new());
    };
    Ok(["name", "displayName"]
        .iter()
        .filter_map(|k| t.attribute(k).map(str::to_owned))
        .collect())
}
fn name_allowed(pkg: &mut PackageEditor<'_>, name: &str, own: Option<&str>) -> Result<(), String> {
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '\\')
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '\\'));
    let upper = name.to_ascii_uppercase();
    let looks_a1 = upper.find(|c: char| c.is_ascii_digit()).is_some_and(|i| {
        i >= 1
            && i <= 3
            && upper[..i].bytes().all(|c| c.is_ascii_uppercase())
            && upper[i..].bytes().all(|c| c.is_ascii_digit())
    });
    let looks_r1c1 = upper
        .strip_prefix('R')
        .and_then(|s| s.split_once('C'))
        .is_some_and(|(r, c)| {
            r.bytes().all(|b| b.is_ascii_digit()) && c.bytes().all(|b| b.is_ascii_digit())
        });
    let looks_cell = looks_a1 || looks_r1c1;
    if !valid || name.encode_utf16().count() > 255 || looks_cell {
        return Err(format!("\"{name}\" is not a valid table name."));
    }
    let lower = name.to_lowercase();
    for path in tables(pkg)? {
        if Some(path.as_str()) == own {
            continue;
        }
        let xml = pkg.read_text(&path)?;
        if names(&xml)?.iter().any(|n| n.to_lowercase() == lower) {
            return Err(format!(
                "Table name \"{name}\" is already taken in this workbook."
            ));
        }
    }
    let wb = pkg.read_text("xl/workbook.xml")?;
    if xml::root_elements(&wb, "definedName")?.iter().any(|e| {
        e.attribute("name")
            .is_some_and(|n| n.to_lowercase() == lower)
    }) {
        return Err(format!(
            "Table name \"{name}\" collides with a defined name in the workbook."
        ));
    }
    Ok(())
}
fn check_columns(name: &str, area: Area, columns: &[Value]) -> Result<Vec<String>, String> {
    let width = area.c2 - area.c1 + 1;
    if width < 0 || columns.len() as i64 != width {
        return Err(format!(
            "Table \"{name}\" spans {width} columns but has {} column names.",
            columns.len()
        ));
    }
    let mut seen = HashSet::new();
    let mut result = Vec::with_capacity(columns.len());
    for v in columns {
        let s = v.as_str().ok_or("Table column name must be a string.")?;
        let key = s.trim().to_lowercase();
        if key.is_empty() {
            return Err(format!("Table \"{name}\" has a blank column name."));
        }
        if !seen.insert(key) {
            return Err(format!(
                "Table \"{name}\" has duplicate column name \"{s}\"."
            ));
        }
        result.push(s.to_owned());
    }
    Ok(result)
}
fn check_overlap(
    pkg: &mut PackageEditor<'_>,
    worksheet: &str,
    name: &str,
    area: Area,
    own: Option<&str>,
    addition: bool,
) -> Result<(), String> {
    for table in sheet_tables(pkg, worksheet)? {
        if Some(table.path.as_str()) == own {
            continue;
        }
        if let Some(t) = first(&table.xml, "table")? {
            if let Some(r) = t.attribute("ref") {
                if area.overlaps(parse_ref(r)?) {
                    return Err(if addition {
                        format!("Table \"{name}\" overlaps an existing table ({r}).")
                    } else {
                        format!("Table \"{name}\" would overlap table {r}.")
                    });
                }
            }
        }
    }
    let sheet = pkg.read_text(worksheet)?;
    if addition {
        if let Some(f) = first(&sheet, "autoFilter")? {
            if let Some(r) = f.attribute("ref") {
                if area.overlaps(parse_ref(r)?) {
                    return Err(format!(
                        "Table \"{name}\" overlaps the sheet auto-filter ({r}) — clear it first."
                    ));
                }
            }
        }
    }
    for m in xml::root_elements(&sheet, "mergeCell")? {
        if let Some(r) = m.attribute("ref") {
            if area.overlaps(parse_ref(r)?) {
                return Err(format!(
                    "Table \"{name}\" {} merged cells ({r}) — unmerge them first.",
                    if addition {
                        "overlaps"
                    } else {
                        "would overlap"
                    }
                ));
            }
        }
    }
    Ok(())
}
fn filter_ref(mut area: Area, totals: i64) -> String {
    area.r2 -= totals;
    area.reference()
}
fn add(
    pkg: &mut PackageEditor<'_>,
    state: &PlannerState,
    v: &Value,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    let sheet = v
        .get("sheetName")
        .and_then(Value::as_str)
        .ok_or("Table needs sheetName.")?;
    let worksheet = state.path_for(pkg, sheet)?;
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .ok_or("Table needs a name.")?;
    let area = Area::from_value(v.get("area").ok_or("Table needs an area.")?)?;
    name_allowed(pkg, name, None)?;
    if area.r1 < 0
        || area.c1 < 0
        || area.r2 < area.r1
        || area.c2 < area.c1
        || area.r2 >= 1048576
        || area.c2 >= 16384
    {
        return Err(format!("Table \"{name}\" lies outside the worksheet grid."));
    }
    let opts = v.get("options").unwrap_or(&Value::Null);
    let header = opts.get("headerRow").and_then(Value::as_bool) != Some(false);
    let totals = opts.get("totalsRow").and_then(Value::as_bool) == Some(true);
    if area.r2 - area.r1 + 1 <= i64::from(header) + i64::from(totals) {
        return Err(format!(
            "Table \"{name}\" needs a header row plus at least one data row."
        ));
    }
    let columns = check_columns(
        name,
        area,
        v.get("columnNames")
            .and_then(Value::as_array)
            .ok_or("Table needs columnNames.")?,
    )?;
    check_overlap(pkg, &worksheet, name, area, None, true)?;
    let mut max_id = 0u64;
    for p in tables(pkg)? {
        let x = pkg.read_text(&p)?;
        if let Some(t) = first(&x, "table")? {
            max_id = max_id.max(t.attribute("id").and_then(|v| v.parse().ok()).unwrap_or(0));
        }
    }
    let removed: HashSet<_> = pkg.removed_paths().into_iter().collect();
    let mut n = 1u64;
    let path = loop {
        let p = format!("xl/tables/table{n}.xml");
        if !removed.contains(&p) && !pkg.has(&p)? {
            break p;
        }
        n += 1;
    };
    let flag = |key: &str| {
        if opts.get(key).and_then(Value::as_bool) == Some(true) {
            "1"
        } else {
            "0"
        }
    };
    let cols = columns
        .iter()
        .enumerate()
        .map(|(i, n)| format!("<tableColumn id=\"{}\" name=\"{}\"/>", i + 1, escape(n)))
        .collect::<String>();
    let filter = if header && opts.get("filterButton").and_then(Value::as_bool) != Some(false) {
        format!(
            "<autoFilter ref=\"{}\"/>",
            filter_ref(area, i64::from(totals))
        )
    } else {
        String::new()
    };
    let style = escape(
        v.get("style")
            .and_then(Value::as_str)
            .unwrap_or("TableStyleMedium2"),
    );
    let band = if v.get("bandedRows").and_then(Value::as_bool) == Some(true) {
        "1"
    } else {
        "0"
    };
    let table = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<table xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" id=\"{}\" name=\"{}\" displayName=\"{}\" ref=\"{}\"{}{}>{filter}<tableColumns count=\"{}\">{cols}</tableColumns><tableStyleInfo name=\"{style}\" showFirstColumn=\"{}\" showLastColumn=\"{}\" showRowStripes=\"{band}\" showColumnStripes=\"{}\"/></table>",
        max_id + 1,
        escape(name),
        escape(name),
        area.reference(),
        if header { "" } else { " headerRowCount=\"0\"" },
        if totals {
            " totalsRowCount=\"1\" totalsRowShown=\"1\""
        } else {
            " totalsRowShown=\"0\""
        },
        columns.len(),
        flag("firstColumn"),
        flag("lastColumn"),
        flag("bandedColumns")
    );
    pkg.add(&path, table);
    touched.insert(path.clone());
    let types = pkg.read_text(CONTENT)?;
    let root = first(&types, "Types")?.ok_or("Invalid content types.")?;
    let item = format!("<Override PartName=\"/{path}\" ContentType=\"{CONTENT_TYPE}\"/>");
    pkg.write(
        CONTENT,
        replace_range(&types, root.close_start, root.close_start, &item),
    )?;
    touched.insert(CONTENT.into());
    let relpath = rels_path(&worksheet)?;
    let rels = if pkg.has(&relpath)? {
        pkg.read_text(&relpath)?.as_ref().clone()
    } else {
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"></Relationships>".into()
    };
    let existing = xml::root_elements(&rels, "Relationship")?;
    let mut rid = 1u64;
    while existing
        .iter()
        .any(|r| r.attribute("Id") == Some(format!("rId{rid}").as_str()))
    {
        rid += 1;
    }
    let rel_id = format!("rId{rid}");
    let relroot = first(&rels, "Relationships")?.ok_or("Invalid relationships.")?;
    let relative = format!("../tables/{}", path.rsplit('/').next().unwrap_or(""));
    let rel = format!("<Relationship Id=\"{rel_id}\" Type=\"{TABLE_REL}\" Target=\"{relative}\"/>");
    let next = replace_range(&rels, relroot.close_start, relroot.close_start, &rel);
    if pkg.has(&relpath)? {
        pkg.write(&relpath, next)?;
    } else {
        pkg.add(&relpath, next);
    }
    touched.insert(relpath);
    let sheet = pkg.read_text(&worksheet)?;
    let mut sheet = sheet.as_ref().clone();
    if let Some(root) = first(&sheet, "worksheet")? {
        if !root.attributes.iter().any(|(k, _)| k == "xmlns:r") {
            let pos = root.open_end - 1;
            sheet.insert_str(
                pos,
                " xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"",
            );
        }
    }
    let child = format!("<tablePart r:id=\"{rel_id}\"/>");
    sheet = if let Some(container) = first(&sheet, "tableParts")? {
        let count = xml::root_elements(&sheet, "tablePart")?.len() + 1;
        let content = if container.close_start == container.end {
            String::new()
        } else {
            sheet[container.open_end..container.close_start].to_owned()
        };
        replace_range(
            &sheet,
            container.start,
            container.end,
            &format!("<tableParts count=\"{count}\">{content}{child}</tableParts>"),
        )
    } else {
        let at = root_tail(&sheet, "worksheet")?;
        replace_range(
            &sheet,
            at,
            at,
            &format!("<tableParts count=\"1\">{child}</tableParts>"),
        )
    };
    pkg.write(&worksheet, sheet)?;
    touched.insert(worksheet);
    Ok(())
}

fn remove(
    pkg: &mut PackageEditor<'_>,
    worksheet: &str,
    table: &LocatedTable,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    let relpath = rels_path(worksheet)?;
    let rels = pkg.read_text(&relpath)?;
    let remove: Vec<_> = xml::root_elements(&rels, "Relationship")?
        .into_iter()
        .filter(|r| r.attribute("Id") == Some(table.rel_id.as_str()))
        .collect();
    pkg.write(&relpath, xml::remove_elements(&rels, &remove))?;
    touched.insert(relpath);
    let sheet = pkg.read_text(worksheet)?;
    let remove: Vec<_> = xml::root_elements(&sheet, "tablePart")?
        .into_iter()
        .filter(|p| p.attribute("r:id") == Some(table.rel_id.as_str()))
        .collect();
    let mut sheet = xml::remove_elements(&sheet, &remove);
    let remaining = xml::root_elements(&sheet, "tablePart")?.len();
    if let Some(parent) = first(&sheet, "tableParts")? {
        if remaining == 0 {
            sheet = replace_range(&sheet, parent.start, parent.end, "");
        } else if parent.attribute("count").is_some() {
            let mut attrs = attributes(&parent);
            set(&mut attrs, "count", Some(remaining.to_string()));
            sheet = replace_range(
                &sheet,
                parent.start,
                parent.open_end,
                &open_tag(&parent.name, &attrs, false),
            );
        }
    }
    pkg.write(worksheet, sheet)?;
    touched.insert(worksheet.into());
    pkg.remove(&table.path);
    touched.insert(table.path.clone());
    let types = pkg.read_text(CONTENT)?;
    let path = format!("/{}", table.path);
    let matching: Vec<_> = xml::root_elements(&types, "Override")?
        .into_iter()
        .filter(|e| e.attribute("PartName") == Some(path.as_str()))
        .collect();
    if !matching.is_empty() {
        pkg.write(CONTENT, xml::remove_elements(&types, &matching))?;
        touched.insert(CONTENT.into());
    }
    Ok(())
}

fn edit(
    pkg: &mut PackageEditor<'_>,
    state: &PlannerState,
    v: &Value,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    let sheet = v
        .get("sheetName")
        .and_then(Value::as_str)
        .ok_or("Table edit needs sheetName.")?;
    let worksheet = state.path_for(pkg, sheet)?;
    let name = v
        .get("tableName")
        .and_then(Value::as_str)
        .ok_or("Table edit needs tableName.")?;
    let mut located = None;
    for t in sheet_tables(pkg, &worksheet)? {
        if names(&t.xml)?
            .iter()
            .any(|n| n.to_lowercase() == name.to_lowercase())
        {
            located = Some(t);
            break;
        }
    }
    let table =
        located.ok_or_else(|| format!("Table \"{name}\" was not found on its worksheet."))?;
    if v.get("remove").and_then(Value::as_bool) == Some(true) {
        return remove(pkg, &worksheet, &table, touched);
    }
    let mut text = table.xml.clone();
    let root =
        first(&text, "table")?.ok_or_else(|| format!("Table part {} is malformed.", table.path))?;
    let mut attrs = attributes(&root);
    let current = root
        .attribute("ref")
        .ok_or_else(|| format!("Table part {} has no ref.", table.path))?;
    let previous = parse_ref(current)?;
    let area = match v.get("area").filter(|v| !v.is_null()) {
        Some(a) => Area::from_value(a)?,
        None => previous,
    };
    if let Some(new) = v.get("name").and_then(Value::as_str) {
        name_allowed(pkg, new, Some(&table.path))?;
        set(&mut attrs, "name", Some(new.into()));
        set(&mut attrs, "displayName", Some(new.into()));
    }
    let header = v
        .get("headerRow")
        .and_then(Value::as_bool)
        .map(i64::from)
        .unwrap_or_else(|| {
            root.attribute("headerRowCount")
                .and_then(|n| n.parse().ok())
                .unwrap_or(1)
        });
    let totals = v
        .get("totalsRow")
        .and_then(Value::as_bool)
        .map(i64::from)
        .unwrap_or_else(|| {
            root.attribute("totalsRowCount")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0)
        });
    if area.r2 - area.r1 + 1 <= header + totals {
        return Err(format!(
            "Table \"{name}\" needs at least one data row beside its header and totals rows."
        ));
    }
    if v.get("area").is_some_and(|v| !v.is_null()) {
        check_overlap(pkg, &worksheet, name, area, Some(&table.path), false)?;
        set(&mut attrs, "ref", Some(area.reference()));
    }
    set(
        &mut attrs,
        "headerRowCount",
        if header == 0 { Some("0".into()) } else { None },
    );
    if totals > 0 {
        set(&mut attrs, "totalsRowCount", Some(totals.to_string()));
        set(&mut attrs, "totalsRowShown", Some("1".into()));
    } else {
        set(&mut attrs, "totalsRowCount", None);
        if v.get("totalsRow").and_then(Value::as_bool) == Some(false) {
            set(&mut attrs, "totalsRowShown", Some("0".into()));
        }
    }
    text = replace_range(
        &text,
        root.start,
        root.open_end,
        &open_tag(&root.name, &attrs, false),
    );
    let block = first(&text, "tableColumns")?
        .ok_or_else(|| format!("Table \"{name}\" has no tableColumns."))?;
    let existing: Vec<_> = xml::root_elements(&text, "tableColumn")?
        .into_iter()
        .filter(|c| c.start >= block.open_end && c.end <= block.close_start)
        .collect();
    let width = area.c2 - area.c1 + 1;
    if let Some(columns) = v.get("columnNames").and_then(Value::as_array) {
        let columns = check_columns(name, area, columns)?;
        let mut max = existing
            .iter()
            .filter_map(|c| c.attribute("id").and_then(|s| s.parse::<u64>().ok()))
            .max()
            .unwrap_or(0);
        let mut next = String::new();
        for (i, column) in columns.iter().enumerate() {
            let previous_index = area.c1 + i as i64 - previous.c1;
            if previous_index >= 0 && (previous_index as usize) < existing.len() {
                let e = &existing[previous_index as usize];
                let mut a = attributes(e);
                set(&mut a, "name", Some(column.clone()));
                let original = &text[e.start..e.end];
                let opener = open_tag(&e.name, &a, e.open_end == e.end);
                next.push_str(&format!("{}{}", opener, &original[e.open_end - e.start..]));
            } else {
                max += 1;
                next.push_str(&format!(
                    "<tableColumn id=\"{max}\" name=\"{}\"/>",
                    escape(column)
                ));
            }
        }
        text = replace_range(
            &text,
            block.start,
            block.end,
            &format!(
                "<tableColumns count=\"{}\">{next}</tableColumns>",
                columns.len()
            ),
        );
    } else if existing.len() as i64 != width {
        return Err(format!(
            "Table \"{name}\" spans {width} columns but lists {} column names.",
            existing.len()
        ));
    }
    let existing_filter = first(&text, "autoFilter")?;
    let want = header > 0
        && v.get("filterButton").and_then(Value::as_bool).unwrap_or(
            v.get("headerRow").and_then(Value::as_bool) == Some(true) || existing_filter.is_some(),
        );
    if let Some(filter) = existing_filter {
        let replacement = if !want {
            String::new()
        } else if area.c1 != previous.c1 || area.c2 != previous.c2 || v.get("headerRow").is_some() {
            format!("<autoFilter ref=\"{}\"/>", filter_ref(area, totals))
        } else {
            let mut a = attributes(&filter);
            set(&mut a, "ref", Some(filter_ref(area, totals)));
            let closed = filter.open_end == filter.end;
            format!(
                "{}{}",
                open_tag(&filter.name, &a, closed),
                &text[filter.open_end..filter.end]
            )
        };
        text = replace_range(&text, filter.start, filter.end, &replacement);
    } else if want {
        let root = first(&text, "table")?.ok_or("Malformed table.")?;
        text = replace_range(
            &text,
            root.open_end,
            root.open_end,
            &format!("<autoFilter ref=\"{}\"/>", filter_ref(area, totals)),
        );
    }
    let style = first(&text, "tableStyleInfo")?;
    let mut a = style.as_ref().map(attributes).unwrap_or_else(|| {
        vec![
            ("showFirstColumn".into(), "0".into()),
            ("showLastColumn".into(), "0".into()),
            ("showRowStripes".into(), "1".into()),
            ("showColumnStripes".into(), "0".into()),
        ]
    });
    if let Some(s) = v.get("style").and_then(Value::as_str) {
        set(&mut a, "name", Some(s.into()));
    }
    for (key, attr) in [
        ("firstColumn", "showFirstColumn"),
        ("lastColumn", "showLastColumn"),
        ("bandedRows", "showRowStripes"),
        ("bandedColumns", "showColumnStripes"),
    ] {
        if let Some(b) = v.get(key).and_then(Value::as_bool) {
            set(&mut a, attr, Some(if b { "1" } else { "0" }.into()));
        }
    }
    let new_style = open_tag("tableStyleInfo", &a, true);
    text = if let Some(style) = style {
        replace_range(&text, style.start, style.end, &new_style)
    } else {
        let at = root_tail(&text, "table")?;
        replace_range(&text, at, at, &new_style)
    };
    pkg.write(&table.path, text)?;
    touched.insert(table.path);
    Ok(())
}

pub(super) fn apply(
    pkg: &mut PackageEditor<'_>,
    state: &mut PlannerState,
    resolved: &Value,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    if let Some(edits) = resolved.get("tableEdits").and_then(Value::as_array) {
        for v in edits {
            edit(pkg, state, v, touched)?;
        }
    }
    if let Some(additions) = resolved.get("tableAdditions").and_then(Value::as_array) {
        for v in additions {
            add(pkg, state, v, touched)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::package::EntrySource;
    use super::*;
    use serde_json::json;
    use std::{collections::HashMap, sync::Arc};
    struct Source(HashMap<String, String>);
    impl EntrySource for Source {
        fn paths(&mut self) -> Result<Vec<String>, String> {
            Ok(self.0.keys().cloned().collect())
        }
        fn read_text(&mut self, path: &str) -> Result<Arc<String>, String> {
            self.0
                .get(path)
                .cloned()
                .map(Arc::new)
                .ok_or_else(|| format!("missing {path}"))
        }
        fn can_patch(&mut self, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn contains_text(&mut self, path: &str, needle: &str) -> Result<bool, String> {
            Ok(self.0.get(path).is_some_and(|s| s.contains(needle)))
        }
        fn short_date_pattern(&mut self) -> Result<String, String> {
            Ok("m/d/yyyy".into())
        }
        fn release_text(&mut self, _: &str) {}
    }
    fn source() -> Source {
        Source(HashMap::from([
        ("xl/workbook.xml".into(),"<workbook xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets><sheet name=\"Sheet1\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>".into()),
        ("xl/_rels/workbook.xml.rels".into(),"<Relationships><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/></Relationships>".into()),
        ("xl/worksheets/sheet1.xml".into(),"<worksheet><sheetData/></worksheet>".into()),
        ("[Content_Types].xml".into(),"<Types></Types>".into()),
    ]))
    }
    #[test]
    fn add_rename_and_remove_update_every_package_part() {
        let mut src = source();
        let mut pkg = PackageEditor::new(&mut src);
        let mut state = PlannerState::new(&mut pkg).unwrap();
        let mut touched = HashSet::new();
        let addition = json!({"sheetName":"Sheet1","name":"Sales","area":{"startRow":0,"endRow":3,"startColumn":0,"endColumn":1},"columnNames":["Name","Value"],"bandedRows":true});
        apply(
            &mut pkg,
            &mut state,
            &json!({"tableAdditions":[addition]}),
            &mut touched,
        )
        .unwrap();
        assert!(
            pkg.read_text("xl/tables/table1.xml")
                .unwrap()
                .contains("ref=\"A1:B4\"")
        );
        assert!(
            pkg.read_text("xl/worksheets/sheet1.xml")
                .unwrap()
                .contains("<tablePart r:id=\"rId1\"/>")
        );
        assert!(
            pkg.read_text(CONTENT)
                .unwrap()
                .contains("/xl/tables/table1.xml")
        );
        let edit = json!({"sheetName":"Sheet1","tableName":"Sales","name":"Revenue","style":"TableStyleLight1","columnNames":["Item","Value"]});
        apply(
            &mut pkg,
            &mut state,
            &json!({"tableEdits":[edit]}),
            &mut touched,
        )
        .unwrap();
        let table = pkg.read_text("xl/tables/table1.xml").unwrap();
        assert!(table.contains("displayName=\"Revenue\""));
        assert!(table.contains("name=\"Item\""));
        assert!(table.contains("TableStyleLight1"));
        apply(
            &mut pkg,
            &mut state,
            &json!({"tableEdits":[{"sheetName":"Sheet1","tableName":"Revenue","remove":true}]}),
            &mut touched,
        )
        .unwrap();
        assert!(!pkg.has("xl/tables/table1.xml").unwrap());
        assert!(
            !pkg.read_text("xl/worksheets/sheet1.xml")
                .unwrap()
                .contains("tableParts")
        );
        assert!(
            !pkg.read_text(CONTENT)
                .unwrap()
                .contains("/xl/tables/table1.xml")
        );
    }
    #[test]
    fn rejects_overlapping_and_duplicate_columns() {
        let area = Area {
            r1: 0,
            c1: 0,
            r2: 3,
            c2: 1,
        };
        assert!(
            check_columns("T", area, &[json!("Name"), json!("name")])
                .unwrap_err()
                .contains("duplicate")
        );
        assert!(area.overlaps(parse_ref("B4:C5").unwrap()));
        assert!(!area.overlaps(parse_ref("C1:D2").unwrap()));
    }
    #[test]
    fn allocation_skips_removed_existing_part() {
        let mut src = source();
        src.0.insert(
            "xl/tables/table1.xml".into(),
            "<table id=\"1\" name=\"Old\" ref=\"A1:B4\"/>".into(),
        );
        let mut pkg = PackageEditor::new(&mut src);
        pkg.remove("xl/tables/table1.xml");
        let mut state = PlannerState::new(&mut pkg).unwrap();
        let mut touched = HashSet::new();
        let addition = json!({"sheetName":"Sheet1","name":"Sales","area":{"startRow":0,"endRow":3,"startColumn":0,"endColumn":1},"columnNames":["Name","Value"],"bandedRows":true});
        apply(
            &mut pkg,
            &mut state,
            &json!({"tableAdditions":[addition]}),
            &mut touched,
        )
        .unwrap();
        assert!(pkg.has("xl/tables/table2.xml").unwrap());
        assert!(!pkg.has("xl/tables/table1.xml").unwrap());
    }
}
