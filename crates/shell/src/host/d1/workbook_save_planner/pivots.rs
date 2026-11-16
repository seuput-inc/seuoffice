use jscompat::TryJsRegex;
use std::collections::HashSet;

use super::package::PackageEditor;
pub(super) mod formula;
mod model;
mod records;
mod relayout;
mod table;
mod validation;

pub(super) fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub(super) fn allocate_part_path(
    pkg: &mut PackageEditor<'_>,
    prefix: &str,
    suffix: &str,
) -> Result<String, String> {
    let mut max = 0_u64;
    for path in pkg.paths()?.into_iter().chain(pkg.removed_paths()) {
        if let Some(number) = path
            .strip_prefix(prefix)
            .and_then(|p| p.strip_suffix(suffix))
            && !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
            && let Ok(value) = number.parse::<u64>()
        {
                    max = max.max(value);
                }
    }
    let next = max
        .checked_add(1)
        .ok_or_else(|| "Part number overflow.".to_owned())?;
    Ok(format!("{prefix}{next}{suffix}"))
}

pub(super) fn rels_path_for(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, name)) => format!("{dir}/_rels/{name}.rels"),
        None => format!("/_rels/{path}.rels"),
    }
}

pub(super) fn relative_target(from: &str, to: &str) -> String {
    let mut from_parts: Vec<_> = from.split('/').collect();
    from_parts.pop();
    let to_parts: Vec<_> = to.split('/').collect();
    let mut common = 0;
    while common < from_parts.len()
        && common + 1 < to_parts.len()
        && from_parts[common] == to_parts[common]
    {
        common += 1;
    }
    format!(
        "{}{}",
        "../".repeat(from_parts.len() - common),
        to_parts[common..].join("/")
    )
}

pub(super) fn append_relationship(
    pkg: &mut PackageEditor<'_>,
    path: &str,
    kind: &str,
    target: &str,
    external: bool,
) -> Result<String, String> {
    let exists = pkg.has(path)?;
    let xml = if exists {
        pkg.read_text(path)?.as_str().to_owned()
    } else {
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"></Relationships>".to_owned()
    };
    let re = regex(r#"\bId\s*=\s*(["'])rId(\d+)\1"#)?;
    let mut used = HashSet::new();
    for found in re.try_find_iter(&xml) {
        let found = found.map_err(|e| e.to_string())?;
        if let Some(number) = found.get(2) {
            used.insert(format!("rId{number}"));
        }
    }
    let mut n = 1_u64;
    while used.contains(&format!("rId{n}")) {
        n = n
            .checked_add(1)
            .ok_or_else(|| "Relationship id overflow.".to_owned())?;
    }
    let id = format!("rId{n}");
    let index = xml
        .rfind("</Relationships>")
        .ok_or_else(|| format!("{path} is not a relationships part."))?;
    let element = format!(
        "<Relationship Id=\"{id}\" Type=\"{}\" Target=\"{}\"{}/>",
        escape_attribute(kind),
        escape_attribute(target),
        if external {
            " TargetMode=\"External\""
        } else {
            ""
        }
    );
    let patched = format!("{}{element}{}", &xml[..index], &xml[index..]);
    if exists {
        pkg.write(path, patched)?;
    } else {
        pkg.add(path, patched);
    }
    Ok(id)
}

pub(super) fn register_content_type_override(
    pkg: &mut PackageEditor<'_>,
    part_path: &str,
    content_type: &str,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    const PATH: &str = "[Content_Types].xml";
    let xml = pkg.read_text(PATH)?;
    let part_name = format!("/{part_path}");
    if xml.contains(&format!("PartName=\"{part_name}\"")) {
        return Ok(());
    }
    let index = xml
        .rfind("</Types>")
        .ok_or_else(|| "[Content_Types].xml is malformed.".to_owned())?;
    let element = format!(
        "<Override PartName=\"{}\" ContentType=\"{}\"/>",
        escape_attribute(&part_name),
        escape_attribute(content_type)
    );
    pkg.write(PATH, format!("{}{element}{}", &xml[..index], &xml[index..]))?;
    touched.insert(PATH.to_owned());
    Ok(())
}

const OFFICE_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PIVOT_TABLE_CONTENT: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.pivotTable+xml";
const PIVOT_CACHE_CONTENT: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.pivotCacheDefinition+xml";
const PIVOT_RECORDS_CONTENT: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.pivotCacheRecords+xml";

fn add_workbook_cache(xml: &str, cache_id: u64, rel_id: &str) -> Result<String, String> {
    let root = regex(r#"<(?![?!])(?:[^>"']|"[^"]*"|'[^']*')*?/?>"#)?;
    let found = root
        .try_find_iter(xml)
        .next()
        .transpose()
        .map_err(|e| e.to_string())?;
    let mut xml = if let Some(found) = found {
        let tag = found.as_str();
        let has_r = regex(r#"\bxmlns:r\s*="#)?
            .try_test(tag)
            .map_err(|e| e.to_string())?;
        if has_r {
            xml.to_owned()
        } else {
            let name_end = tag
                .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .unwrap_or(tag.len());
            let tag = format!(
                "{} xmlns:r=\"{OFFICE_REL}\"{}",
                &tag[..name_end],
                &tag[name_end..]
            );
            format!("{}{}{}", &xml[..found.start()], tag, &xml[found.end()..])
        }
    } else {
        xml.to_owned()
    };
    let entry = format!("<pivotCache cacheId=\"{cache_id}\" r:id=\"{rel_id}\"/>");
    if let Some(index) = xml.find("</pivotCaches>") {
        xml.insert_str(index, &entry);
        return Ok(xml);
    }
    let element = format!("<pivotCaches>{entry}</pivotCaches>");
    let ext = regex(r#"<extLst\b"#)?;
    let ext_at = ext
        .try_find_iter(&xml)
        .next()
        .transpose()
        .map_err(|e| e.to_string())?
        .map(|m| m.start());
    let index = ext_at
        .or_else(|| xml.rfind("</workbook>"))
        .ok_or_else(|| "xl/workbook.xml is malformed.".to_owned())?;
    xml.insert_str(index, &element);
    Ok(xml)
}

/// Installs already-built pivot parts and returns the updated workbook XML.
/// Cache XML must be built after its records relationship id is allocated.
#[allow(clippy::too_many_arguments)]
pub(super) fn install_pivot_parts<F>(
    pkg: &mut PackageEditor<'_>,
    worksheet_path: &str,
    workbook_xml: &str,
    cache_id: u64,
    records_xml: String,
    table_xml: String,
    cache_xml: F,
    touched: &mut HashSet<String>,
) -> Result<String, String>
where
    F: FnOnce(&str) -> Result<String, String>,
{
    let records_path = allocate_part_path(pkg, "xl/pivotCache/pivotCacheRecords", ".xml")?;
    pkg.add(&records_path, records_xml);
    touched.insert(records_path.clone());
    register_content_type_override(pkg, &records_path, PIVOT_RECORDS_CONTENT, touched)?;

    let cache_path = allocate_part_path(pkg, "xl/pivotCache/pivotCacheDefinition", ".xml")?;
    let cache_rels = rels_path_for(&cache_path);
    let records_id = append_relationship(
        pkg,
        &cache_rels,
        &format!("{OFFICE_REL}/pivotCacheRecords"),
        &relative_target(&cache_path, &records_path),
        false,
    )?;
    touched.insert(cache_rels);
    pkg.add(&cache_path, cache_xml(&records_id)?);
    touched.insert(cache_path.clone());
    register_content_type_override(pkg, &cache_path, PIVOT_CACHE_CONTENT, touched)?;

    let table_path = allocate_part_path(pkg, "xl/pivotTables/pivotTable", ".xml")?;
    let table_rels = rels_path_for(&table_path);
    append_relationship(
        pkg,
        &table_rels,
        &format!("{OFFICE_REL}/pivotCacheDefinition"),
        &relative_target(&table_path, &cache_path),
        false,
    )?;
    touched.insert(table_rels);
    pkg.add(&table_path, table_xml);
    touched.insert(table_path.clone());
    register_content_type_override(pkg, &table_path, PIVOT_TABLE_CONTENT, touched)?;

    let sheet_rels = rels_path_for(worksheet_path);
    append_relationship(
        pkg,
        &sheet_rels,
        &format!("{OFFICE_REL}/pivotTable"),
        &relative_target(worksheet_path, &table_path),
        false,
    )?;
    touched.insert(sheet_rels);
    let workbook_rels = "xl/_rels/workbook.xml.rels";
    let id = append_relationship(
        pkg,
        workbook_rels,
        &format!("{OFFICE_REL}/pivotCacheDefinition"),
        &relative_target("xl/workbook.xml", &cache_path),
        false,
    )?;
    touched.insert(workbook_rels.to_owned());
    add_workbook_cache(workbook_xml, cache_id, &id)
}

pub(super) struct CacheField<'a> {
    pub name: &'a str,
    /// None means a value field; Some is the dimension's shared item list.
    pub items: Option<&'a [String]>,
}

pub(super) struct CalculatedCacheField<'a> {
    pub name: &'a str,
    /// Formula already parsed and normalized against the source fields.
    pub formula: &'a str,
}

pub(super) fn build_cache_definition(
    records_id: &str,
    source_ref: &str,
    source_sheet: &str,
    fields: &[CacheField<'_>],
    calculated: &[CalculatedCacheField<'_>],
    record_count: usize,
) -> String {
    let mut body = String::new();
    for field in fields {
        let items = match field.items {
            Some(items) if !items.is_empty() => {
                let mut children = String::new();
                for item in items {
                    children.push_str(&format!("<s v=\"{}\"/>", escape_attribute(item)));
                }
                format!(
                    "<sharedItems count=\"{}\">{children}</sharedItems>",
                    items.len()
                )
            }
            _ => "<sharedItems/>".to_owned(),
        };
        body.push_str(&format!(
            "<cacheField name=\"{}\" numFmtId=\"0\">{items}</cacheField>",
            escape_attribute(field.name)
        ));
    }
    for field in calculated {
        body.push_str(&format!(
            "<cacheField name=\"{}\" formula=\"{}\" databaseField=\"0\" numFmtId=\"0\"/>",
            escape_attribute(field.name),
            escape_attribute(field.formula)
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<pivotCacheDefinition xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"{OFFICE_REL}\" r:id=\"{records_id}\" refreshedBy=\"seuoffice\" createdVersion=\"8\" refreshedVersion=\"8\" minRefreshableVersion=\"3\" recordCount=\"{record_count}\"><cacheSource type=\"worksheet\"><worksheetSource ref=\"{source_ref}\" sheet=\"{}\"/></cacheSource><cacheFields count=\"{}\">{body}</cacheFields></pivotCacheDefinition>",
        escape_attribute(source_sheet),
        fields.len() + calculated.len()
    )
}

pub(super) enum PivotFilter<'a> {
    Label {
        field: f64,
        op: LabelOp,
        value: &'a str,
    },
    Top {
        field: f64,
        data_field: f64,
        count: f64,
    },
    Greater {
        field: f64,
        data_field: f64,
        from: f64,
    },
    Between {
        field: f64,
        data_field: f64,
        from: f64,
        to: f64,
    },
}

pub(super) enum LabelOp {
    Equal,
    Contains,
    BeginsWith,
}

pub(super) fn build_pivot_filters(filters: &[PivotFilter<'_>]) -> String {
    if filters.is_empty() {
        return String::new();
    }
    let mut entries = String::new();
    for (index, filter) in filters.iter().enumerate() {
        let id = index + 1;
        let number = jscompat::number_to_string;
        let entry = match filter {
            PivotFilter::Label { field, op, value } => {
                let (kind, wildcard) = match op {
                    LabelOp::Equal => ("captionEqual", (*value).to_owned()),
                    LabelOp::Contains => ("captionContains", format!("*{value}*")),
                    LabelOp::BeginsWith => ("captionBeginsWith", format!("{value}*")),
                };
                format!(
                    "<filter fld=\"{}\" type=\"{kind}\" evalOrder=\"-1\" id=\"{id}\" stringValue1=\"{}\"><autoFilter ref=\"A1\"><filterColumn colId=\"0\"><customFilters><customFilter val=\"{}\"/></customFilters></filterColumn></autoFilter></filter>",
                    number(*field),
                    escape_attribute(value),
                    escape_attribute(&wildcard)
                )
            }
            PivotFilter::Top {
                field,
                data_field,
                count,
            } => format!(
                "<filter fld=\"{}\" type=\"count\" evalOrder=\"-1\" id=\"{id}\" iMeasureFld=\"{}\"><autoFilter ref=\"A1\"><filterColumn colId=\"0\"><top10 val=\"{}\"/></filterColumn></autoFilter></filter>",
                number(*field),
                number(*data_field),
                number(*count)
            ),
            PivotFilter::Greater {
                field,
                data_field,
                from,
            } => format!(
                "<filter fld=\"{}\" type=\"valueGreaterThan\" evalOrder=\"-1\" id=\"{id}\" iMeasureFld=\"{}\"><autoFilter ref=\"A1\"><filterColumn colId=\"0\"><customFilters><customFilter operator=\"greaterThan\" val=\"{}\"/></customFilters></filterColumn></autoFilter></filter>",
                number(*field),
                number(*data_field),
                number(*from)
            ),
            PivotFilter::Between {
                field,
                data_field,
                from,
                to,
            } => format!(
                "<filter fld=\"{}\" type=\"valueBetween\" evalOrder=\"-1\" id=\"{id}\" iMeasureFld=\"{}\"><autoFilter ref=\"A1\"><filterColumn colId=\"0\"><customFilters and=\"1\"><customFilter operator=\"greaterThanOrEqual\" val=\"{}\"/><customFilter operator=\"lessThanOrEqual\" val=\"{}\"/></customFilters></filterColumn></autoFilter></filter>",
                number(*field),
                number(*data_field),
                number(*from),
                number(*to)
            ),
        };
        entries.push_str(&entry);
    }
    format!("<filters count=\"{}\">{entries}</filters>", filters.len())
}

pub(super) struct AxisLine {
    pub kind: String,
    pub members: Vec<usize>,
}

pub(super) fn build_axis_items(lines: &[AxisLine], levels: usize, tag: &str) -> String {
    let mut carried: Vec<Option<usize>> = vec![None; levels];
    let mut children = String::new();
    for line in lines {
        let mut repeat = 0;
        while repeat + 1 < line.members.len()
            && carried.get(repeat).copied().flatten() == Some(line.members[repeat])
        {
            repeat += 1;
        }
        let repeat_attr = if repeat == 0 {
            String::new()
        } else {
            format!(" r=\"{repeat}\"")
        };
        let kind_attr = if line.kind == "data" {
            String::new()
        } else {
            format!(" t=\"{}\"", line.kind)
        };
        children.push_str(&format!("<i{repeat_attr}{kind_attr}>"));
        for &member in &line.members[repeat..] {
            if member == 0 {
                children.push_str("<x/>");
            } else {
                children.push_str(&format!("<x v=\"{member}\"/>"));
            }
        }
        children.push_str("</i>");
        carried.clear();
        carried.extend(line.members.iter().copied().map(Some));
        carried.resize(levels, None);
    }
    format!(
        "<{tag} count=\"{}\">{children}<i t=\"grand\"><x/></i></{tag}>",
        lines.len() + 1
    )
}

pub(super) fn build_axis_pivot_field(
    axis: &str,
    item_count: usize,
    tabular: bool,
    hidden: &HashSet<usize>,
) -> String {
    let mut items = String::new();
    for index in 0..item_count {
        items.push_str(&format!(
            "<item x=\"{index}\"{}/>",
            if hidden.contains(&index) {
                " h=\"1\""
            } else {
                ""
            }
        ));
    }
    format!(
        "<pivotField axis=\"{axis}\" showAll=\"0\"{}><items count=\"{}\">{items}<item t=\"default\"/></items></pivotField>",
        if tabular {
            " compact=\"0\" outline=\"0\""
        } else {
            ""
        },
        item_count + 1
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cell {
    col: i64,
    row: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Area {
    start: Cell,
    end: Cell,
}

fn parse_cell(input: &str) -> Result<Cell, String> {
    let value = input.trim().to_ascii_uppercase();
    let split = value.bytes().take_while(u8::is_ascii_uppercase).count();
    let (letters, digits) = value.split_at(split);
    if letters.is_empty() || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("Cannot parse cell address \"{input}\"."));
    }
    let mut col = 0_i64;
    for b in letters.bytes() {
        col = col
            .checked_mul(26)
            .and_then(|n| n.checked_add(i64::from(b - b'A' + 1)))
            .ok_or_else(|| format!("Cannot parse cell address \"{input}\"."))?;
    }
    let row = digits
        .parse::<i64>()
        .map_err(|_| format!("Cannot parse cell address \"{input}\"."))?;
    Ok(Cell {
        col: col - 1,
        row: row - 1,
    })
}

fn parse_area(input: &str) -> Result<Area, String> {
    let value = input.trim().to_ascii_uppercase();
    let mut parts = value.split(':');
    let first = parts.next().unwrap_or("");
    let Some(second) = parts.next() else {
        return Err(format!("Cannot parse area ref \"{input}\"."));
    };
    if parts.next().is_some() {
        return Err(format!("Cannot parse area ref \"{input}\"."));
    }
    Ok(Area {
        start: parse_cell(first)?,
        end: parse_cell(second)?,
    })
}

fn cell_label(cell: Cell) -> String {
    let mut col = cell.col + 1;
    let mut letters = Vec::new();
    while col > 0 {
        col -= 1;
        letters.push((b'A' + (col % 26) as u8) as char);
        col /= 26;
    }
    format!(
        "{}{}",
        letters.iter().rev().collect::<String>(),
        cell.row + 1
    )
}

fn area_label(area: Area) -> String {
    format!("{}:{}", cell_label(area.start), cell_label(area.end))
}

fn added_areas(old: Area, new: Area) -> Vec<Area> {
    let mut result = Vec::new();
    if old.end.row < new.end.row {
        result.push(Area {
            start: Cell {
                col: new.start.col,
                row: old.end.row + 1,
            },
            end: new.end,
        });
    }
    if old.end.col < new.end.col {
        result.push(Area {
            start: Cell {
                col: old.end.col + 1,
                row: new.start.row,
            },
            end: Cell {
                col: new.end.col,
                row: old.end.row.min(new.end.row),
            },
        });
    }
    result
}

fn regex(pattern: &str) -> Result<TryJsRegex, String> {
    TryJsRegex::new(pattern, "").map_err(|e| e.to_string())
}

fn has_content(xml: &str, area: Area) -> Result<bool, String> {
    let rows = regex(r#"<row\b([^>]*?)>([\s\S]*?)</row>"#)?;
    let row_ref = regex(r#"\br="(\d+)""#)?;
    let cells = regex(r#"<c\b([^>]*?)(?:/>|>[\s\S]*?</c>)"#)?;
    let cell_ref = regex(r#"\br="([A-Z]+\d+)""#)?;
    let content = regex(r#"<v[^>]*>|<is>"#)?;
    for row in rows.try_find_iter(xml) {
        let row = row.map_err(|e| e.to_string())?;
        let attrs = row.get(1).unwrap_or("");
        let Some(r) = row_ref
            .try_find_iter(attrs)
            .next()
            .transpose()
            .map_err(|e| e.to_string())?
        else {
            continue;
        };
        let Some(index) = r.get(1).and_then(|v| v.parse::<i64>().ok()) else {
            continue;
        };
        let index = index - 1;
        if index < area.start.row || index > area.end.row {
            continue;
        }
        for cell in cells.try_find_iter(row.get(2).unwrap_or("")) {
            let cell = cell.map_err(|e| e.to_string())?;
            let Some(address) = cell_ref
                .try_find_iter(cell.get(1).unwrap_or(""))
                .next()
                .transpose()
                .map_err(|e| e.to_string())?
            else {
                continue;
            };
            let Some(reference) = address.get(1) else {
                continue;
            };
            let parsed = parse_cell(reference)?;
            if parsed.col < area.start.col
                || parsed.col > area.end.col
                || cell.as_str().ends_with("/>")
            {
                continue;
            }
            if content.try_test(cell.as_str()).map_err(|e| e.to_string())? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Validates the newly occupied cells before updating a pivot's location.
/// The caller resolves the pivot and worksheet and handles any relayout separately.
pub(super) fn validate_pivot_growth(
    current_ref: &str,
    new_ref: &str,
    worksheet_xml: Option<&str>,
) -> Result<(), String> {
    let old = parse_area(current_ref)?;
    let new = parse_area(new_ref)?;
    if old.start != new.start {
        return Err(format!(
            "Pivot \"{new_ref}\" cannot change its top-left cell during refresh."
        ));
    }
    if new.end.row > old.end.row || new.end.col > old.end.col {
        let xml = worksheet_xml
            .ok_or_else(|| "Pivot expansion needs a resolved worksheet path.".to_owned())?;
        for area in added_areas(old, new) {
            if has_content(xml, area)? {
                return Err(format!(
                    "Pivot expansion into {} conflicts with existing worksheet content. Clear the area first or move the pivot.",
                    area_label(area)
                ));
            }
        }
    }
    Ok(())
}

/// Replaces only the location reference, preserving the rest of the pivot part.
pub(super) fn update_pivot_location(xml: &str, new_ref: &str) -> Result<String, String> {
    let location = regex(r#"<location\b([^>]*)\bref="[^"]*"([^>]*)>"#)?;
    let Some(found) = location
        .try_find_iter(xml)
        .next()
        .transpose()
        .map_err(|e| e.to_string())?
    else {
        return Ok(xml.to_owned());
    };
    let mut output = String::with_capacity(xml.len() + new_ref.len());
    output.push_str(&xml[..found.start()]);
    output.push_str("<location");
    output.push_str(found.get(1).unwrap_or(""));
    output.push_str("ref=\"");
    output.push_str(new_ref);
    output.push('"');
    output.push_str(found.get(2).unwrap_or(""));
    output.push('>');
    output.push_str(&xml[found.end()..]);
    Ok(output)
}

fn pivot_table_for_cache(
    pkg: &mut PackageEditor<'_>,
    cache_path: &str,
) -> Result<Option<String>, String> {
    let tag_re = regex(r#"<Relationship\b[^>]*>"#)?;
    let target_re = regex(r#"\bTarget="([^"]+)""#)?;
    let type_re = regex(r#"\bType="[^"]*pivotCacheDefinition[^"]*""#)?;
    for rels_path in pkg.paths()? {
        let Some(filename) = rels_path.strip_prefix("xl/pivotTables/_rels/") else {
            continue;
        };
        let Some(filename) = filename.strip_suffix(".rels") else {
            continue;
        };
        if filename.contains('/') {
            continue;
        }
        let table_path = format!("xl/pivotTables/{filename}");
        let xml = pkg.read_text(&rels_path)?;
        for tag in tag_re.try_find_iter(&xml) {
            let tag = tag.map_err(|e| e.to_string())?;
            if !type_re.try_test(tag.as_str()).map_err(|e| e.to_string())? {
                continue;
            }
            let Some(target) = target_re
                .try_find_iter(tag.as_str())
                .next()
                .transpose()
                .map_err(|e| e.to_string())?
            else {
                continue;
            };
            let Some(target) = target.get(1) else {
                continue;
            };
            if super::state::resolve_rel_target(&table_path, target)? == cache_path {
                return Ok(Some(table_path));
            }
        }
    }
    Ok(None)
}

pub(super) fn apply_refresh_phase(
    pkg: &mut PackageEditor<'_>,
    state: &mut super::state::PlannerState,
    resolved: &serde_json::Value,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    let Some(updates) = resolved
        .get("pivotRefreshUpdates")
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(());
    };
    let mut paths = Vec::with_capacity(updates.len());
    for update in updates {
        let path = if let Some(path) = update
            .get("worksheetPath")
            .and_then(serde_json::Value::as_str)
        {
            Some(path.to_owned())
        } else if let Some(name) = update.get("sheetName").and_then(serde_json::Value::as_str) {
            Some(state.path_for(pkg, name)?)
        } else {
            None
        };
        paths.push(path);
    }
    for (update, worksheet) in updates.iter().zip(paths) {
        let cache = update
            .get("cachePath")
            .and_then(serde_json::Value::as_str)
            .ok_or("Pivot refresh is missing cachePath.")?;
        let new_ref = update
            .get("newOutputRef")
            .and_then(serde_json::Value::as_str)
            .ok_or("Pivot refresh is missing newOutputRef.")?;
        let Some(path) = pivot_table_for_cache(pkg, cache)? else {
            continue;
        };
        let xml = pkg.read_text(&path)?;
        let location = regex(r#"<location\b[^>]*\bref="([^"]+)""#)?;
        let Some(found) = location
            .try_find_iter(&xml)
            .next()
            .transpose()
            .map_err(|e| e.to_string())?
        else {
            continue;
        };
        let Some(current_ref) = found.get(1) else {
            continue;
        };
        let old = parse_area(current_ref)?;
        let new = parse_area(new_ref)?;
        let relayout = update.get("relayout").filter(|value| !value.is_null());
        if old == new && relayout.is_none() {
            continue;
        }
        if old.start != new.start {
            return Err(format!(
                "Pivot \"{new_ref}\" cannot change its top-left cell during refresh."
            ));
        }
        let worksheet_xml = if new.end.row > old.end.row || new.end.col > old.end.col {
            let worksheet = worksheet
                .as_deref()
                .ok_or_else(|| "Pivot expansion needs a resolved worksheet path.".to_owned())?;
            Some(pkg.read_text(worksheet)?)
        } else {
            None
        };
        validate_pivot_growth(
            current_ref,
            new_ref,
            worksheet_xml.as_deref().map(String::as_str),
        )?;
        if let Some(layout) = relayout {
            let mut addition = layout.clone();
            let object = addition.as_object_mut().ok_or("Invalid pivot relayout.")?;
            object.insert(
                "location".into(),
                serde_json::json!({
                    "startRow": new.start.row, "startColumn": new.start.col,
                    "endRow": new.end.row, "endColumn": new.end.col,
                }),
            );
            if let Some(path) = worksheet.as_deref() {
                object.insert("worksheetPath".into(), path.into());
            }
            let short_date = pkg.short_date_pattern()?;
            relayout::apply_parts(
                pkg,
                &path,
                &xml,
                cache,
                touched,
                |name, cache_id| {
                    let mut addition = addition.clone();
                    addition["name"] = name.into();
                    model::Model::parse_unchecked(&addition)?.table_xml(cache_id, &short_date)
                },
                |records_id| model::Model::parse_unchecked(&addition)?.cache_xml(records_id, 0),
            )?;
        } else {
            pkg.write(&path, update_pivot_location(&xml, new_ref)?)?;
            touched.insert(path);
        }
    }
    Ok(())
}

fn refresh_on_load(xml: &str) -> Result<String, String> {
    let root = regex(r#"<pivotCacheDefinition\b[^>]*>"#)?;
    let Some(found) = root
        .try_find_iter(xml)
        .next()
        .transpose()
        .map_err(|e| e.to_string())?
    else {
        return Err("pivotCacheDefinition part has no root element.".to_owned());
    };
    let tag = found.as_str();
    let updated = if tag.contains(" refreshOnLoad=\"") {
        let attr = regex(r#" refreshOnLoad="[^"]*""#)?;
        if let Some(value) = attr
            .try_find_iter(tag)
            .next()
            .transpose()
            .map_err(|e| e.to_string())?
        {
            format!(
                "{} refreshOnLoad=\"1\"{}",
                &tag[..value.start()],
                &tag[value.end()..]
            )
        } else {
            tag.to_owned()
        }
    } else {
        tag.replacen(
            "<pivotCacheDefinition",
            "<pivotCacheDefinition refreshOnLoad=\"1\"",
            1,
        )
    };
    Ok(format!(
        "{}{}{}",
        &xml[..found.start()],
        updated,
        &xml[found.end()..]
    ))
}

pub(super) fn refresh_caches(
    pkg: &mut PackageEditor<'_>,
    paths: &[String],
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    for path in paths {
        let xml = pkg.read_text(path)?;
        pkg.write(path, refresh_on_load(&xml)?)?;
        touched.insert(path.clone());
    }
    Ok(())
}

pub(super) fn apply_additions_phase(
    pkg: &mut PackageEditor<'_>,
    state: &mut super::state::PlannerState,
    resolved: &serde_json::Value,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    let Some(additions) = resolved
        .get("pivotAdditions")
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(());
    };
    if additions.is_empty() {
        return Ok(());
    }
    let mut worksheet_paths = Vec::with_capacity(additions.len());
    for addition in additions {
        let sheet = addition
            .get("sheetName")
            .and_then(serde_json::Value::as_str)
            .ok_or("Pivot is missing sheetName.")?;
        worksheet_paths.push(state.path_for(pkg, sheet)?);
    }
    let name_re = regex(r#"<pivotTableDefinition\b[^>]*\bname="([^"]+)""#)?;
    let mut names = HashSet::new();
    for path in pkg.paths()? {
        let Some(name) = path.strip_prefix("xl/pivotTables/") else {
            continue;
        };
        if name.contains('/') || !name.ends_with(".xml") {
            continue;
        }
        let xml = pkg.read_text(&path)?;
        if let Some(found) = name_re
            .try_find_iter(&xml)
            .next()
            .transpose()
            .map_err(|e| e.to_string())?
            && let Some(name) = found.get(1)
        {
                names.insert(name.to_lowercase());
            }
    }
    let cache_re = regex(r#"<pivotCache\b[^>]*\bcacheId="(\d+)""#)?;
    let mut cache_id = 0_u64;
    for found in cache_re.try_find_iter(&state.workbook_xml) {
        let found = found.map_err(|e| e.to_string())?;
        if let Some(id) = found.get(1).and_then(|v| v.parse::<u64>().ok()) {
            cache_id = cache_id.max(id);
        }
    }
    let short_date = pkg.short_date_pattern()?;
    let mut workbook = state.workbook_xml.as_str().to_owned();
    for (addition, worksheet_path) in additions.iter().zip(&worksheet_paths) {
        let model = model::Model::parse(addition)?;
        let name = addition
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or("Pivot is missing name.")?;
        if !names.insert(name.to_lowercase()) {
            return Err(format!(
                "Pivot name \"{name}\" is already taken in this workbook."
            ));
        }
        cache_id = cache_id.checked_add(1).ok_or("Pivot cache id overflow.")?;
        let source_path = state.path_for(pkg, model.source_sheet()?)?;
        let source_xml = pkg.read_text(&source_path)?;
        let strings = if pkg.has("xl/sharedStrings.xml")? {
            records::shared_strings(&pkg.read_text("xl/sharedStrings.xml")?)?
        } else {
            Vec::new()
        };
        let (r1, c1, r2, c2) = model.source_area()?;
        let source_rows = records::source_rows(&source_xml, &strings, r1, c1, r2, c2)?;
        let records = records::build_records(&source_rows, &model)?;
        let table = model.table_xml(cache_id, &short_date)?;
        workbook = install_pivot_parts(
            pkg,
            worksheet_path,
            &workbook,
            cache_id,
            records,
            table,
            |records_id| model.cache_xml(records_id, source_rows.len()),
            touched,
        )?;
    }
    state.workbook_xml = std::sync::Arc::new(workbook);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::package::{EntrySource, PackageEditor};
    use super::{refresh_on_load, update_pivot_location, validate_pivot_growth};
    use std::{collections::HashSet, sync::Arc};

    struct Source(Vec<(String, String)>);
    impl EntrySource for Source {
        fn paths(&mut self) -> Result<Vec<String>, String> {
            Ok(self.0.iter().map(|(path, _)| path.clone()).collect())
        }
        fn read_text(&mut self, path: &str) -> Result<Arc<String>, String> {
            self.0
                .iter()
                .find(|(name, _)| name == path)
                .map(|(_, xml)| Arc::new(xml.clone()))
                .ok_or_else(|| format!("missing {path}"))
        }
        fn can_patch(&mut self, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn contains_text(&mut self, path: &str, needle: &str) -> Result<bool, String> {
            Ok(self.read_text(path)?.contains(needle))
        }
        fn release_text(&mut self, _: &str) {}
        fn short_date_pattern(&mut self) -> Result<String, String> {
            Ok("m/d/yyyy".into())
        }
    }

    #[test]
    fn additions_and_refresh_mutate_linked_pivot_parts() {
        let workbook =
            r#"<workbook><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let mut source = Source(vec![
            ("[Content_Types].xml".into(), "<Types></Types>".into()),
            ("xl/workbook.xml".into(), workbook.into()),
            ("xl/_rels/workbook.xml.rels".into(), r#"<Relationships><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#.into()),
            ("xl/worksheets/sheet1.xml".into(), r#"<worksheet><sheetData><row r="2"><c r="A2" t="inlineStr"><is><t>Paris</t></is></c><c r="B2"><v>4</v></c></row></sheetData></worksheet>"#.into()),
        ]);
        let mut pkg = PackageEditor::new(&mut source);
        let mut state = super::super::state::PlannerState::new(&mut pkg).unwrap();
        let mut touched = HashSet::new();
        let request = serde_json::json!({"pivotAdditions":[{
            "sheetName":"Data", "sourceSheetName":"Data", "name":"Sales",
            "sourceArea":{"startRow":0,"startColumn":0,"endRow":1,"endColumn":1},
            "location":{"startRow":0,"startColumn":3,"endRow":2,"endColumn":4},
            "fieldNames":["City","Amount"], "rowFieldIndices":[0], "rowItems":["Paris"],
            "values":[{"fieldIndex":1,"agg":"sum"}]
        }]});
        super::apply_additions_phase(&mut pkg, &mut state, &request, &mut touched).unwrap();
        assert!(
            pkg.read_text("xl/pivotCache/pivotCacheRecords1.xml")
                .unwrap()
                .contains(r#"<r><x v="0"/><n v="4"/></r>"#)
        );
        assert!(
            pkg.read_text("xl/pivotTables/pivotTable1.xml")
                .unwrap()
                .contains(r#"<location ref="D1:E3""#)
        );
        assert!(
            state
                .workbook_xml
                .contains(r#"<pivotCache cacheId="1" r:id="rId2"/>"#)
        );
        let refresh = serde_json::json!({"pivotRefreshUpdates":[{"cachePath":"xl/pivotCache/pivotCacheDefinition1.xml","sheetName":"Data","newOutputRef":"D1:E4"}]});
        super::apply_refresh_phase(&mut pkg, &mut state, &refresh, &mut touched).unwrap();
        assert!(
            pkg.read_text("xl/pivotTables/pivotTable1.xml")
                .unwrap()
                .contains(r#"<location ref="D1:E4""#)
        );
        let layout = request["pivotAdditions"][0].clone();
        let refresh = serde_json::json!({"pivotRefreshUpdates":[{"cachePath":"xl/pivotCache/pivotCacheDefinition1.xml","sheetName":"Data","newOutputRef":"D1:F4","relayout":layout}]});
        super::apply_refresh_phase(&mut pkg, &mut state, &refresh, &mut touched).unwrap();
        let table = pkg.read_text("xl/pivotTables/pivotTable1.xml").unwrap();
        assert!(table.contains(r#"name="Sales" cacheId="1""#));
        assert!(table.contains(r#"<location ref="D1:F4""#));
        let records = pkg
            .read_text("xl/pivotCache/pivotCacheRecords1.xml")
            .unwrap();
        assert!(records.contains(r#"count="0"/>"#));
        assert!(
            pkg.read_text("xl/pivotCache/pivotCacheDefinition1.xml")
                .unwrap()
                .contains(r#"r:id="rId1""#)
        );
    }

    #[test]
    fn multilevel_axis_compresses_only_the_shared_prefix() {
        let lines = vec![
            super::AxisLine {
                kind: "data".into(),
                members: vec![0, 0],
            },
            super::AxisLine {
                kind: "data".into(),
                members: vec![0, 1],
            },
            super::AxisLine {
                kind: "default".into(),
                members: vec![0],
            },
            super::AxisLine {
                kind: "data".into(),
                members: vec![1, 0],
            },
        ];
        assert_eq!(
            super::build_axis_items(&lines, 2, "rowItems"),
            r#"<rowItems count="5"><i><x/><x/></i><i r="1"><x v="1"/></i><i t="default"><x/></i><i><x v="1"/><x/></i><i t="grand"><x/></i></rowItems>"#
        );
    }

    #[test]
    fn pivot_install_wires_parts_and_keeps_existing_relationship_ids() {
        let mut source = Source(vec![
            ("[Content_Types].xml".into(), "<Types></Types>".into()),
            ("xl/_rels/workbook.xml.rels".into(), r#"<Relationships><Relationship Id='rId1'/><Relationship Id="rId3"/></Relationships>"#.into()),
        ]);
        let mut pkg = PackageEditor::new(&mut source);
        let mut touched = HashSet::new();
        let workbook = super::install_pivot_parts(
            &mut pkg,
            "xl/worksheets/sheet1.xml",
            "<workbook><sheets/></workbook>",
            7,
            "<records/>".into(),
            "<pivot/>".into(),
            |id| Ok(format!("<cache r:id=\"{id}\"/>")),
            &mut touched,
        )
        .unwrap();
        assert!(workbook.contains(r#"<pivotCache cacheId="7" r:id="rId2"/>"#));
        let plan = pkg.into_plan(touched);
        let lookup = |path: &str| {
            plan.added
                .iter()
                .chain(plan.replaced.iter())
                .find(|(name, _)| name == path)
                .map(|(_, xml)| xml.as_str())
                .unwrap()
        };
        assert!(
            lookup("xl/worksheets/_rels/sheet1.xml.rels")
                .contains(r#"Target="../pivotTables/pivotTable1.xml""#)
        );
        assert!(
            lookup("xl/pivotTables/_rels/pivotTable1.xml.rels")
                .contains(r#"Target="../pivotCache/pivotCacheDefinition1.xml""#)
        );
        assert!(lookup("xl/pivotCache/pivotCacheDefinition1.xml").contains(r#"r:id="rId1""#));
        assert!(
            lookup("[Content_Types].xml")
                .contains(r#"PartName="/xl/pivotCache/pivotCacheRecords1.xml""#)
        );
    }

    #[test]
    fn pivot_filters_escape_labels() {
        let filters = [super::PivotFilter::Label {
            field: 0.0,
            op: super::LabelOp::Contains,
            value: "A&B",
        }];
        let xml = super::build_pivot_filters(&filters);
        assert!(xml.contains(r#"type="captionContains""#));
        assert!(xml.contains(r#"<customFilter val="*A&amp;B*"/>"#));
    }

    #[test]
    fn expansion_detects_new_content_but_not_old_or_empty_cells() {
        let xml = r#"<worksheet><sheetData><row r="2"><c r="B2"><v>old</v></c><c r="D2"/><c r="E2"><v>other</v></c></row><row r="4"><c r="C4" t="inlineStr"><is><t>occupied</t></is></c></row></sheetData></worksheet>"#;
        assert!(validate_pivot_growth("B2:C3", "B2:D3", Some(xml)).is_ok());
        assert_eq!(
            validate_pivot_growth("B2:C3", "B2:D4", Some(xml)).unwrap_err(),
            "Pivot expansion into B4:D4 conflicts with existing worksheet content. Clear the area first or move the pivot."
        );
    }

    #[test]
    fn location_update_preserves_other_pivot_content() {
        let xml = r#"<pivotTableDefinition name="Sales"><location firstHeaderRow="1" ref="A1:B3" firstDataRow="2"/><pivotFields count="0"/></pivotTableDefinition>"#;
        assert_eq!(
            update_pivot_location(xml, "A1:C5").unwrap(),
            r#"<pivotTableDefinition name="Sales"><location firstHeaderRow="1" ref="A1:C5" firstDataRow="2"/><pivotFields count="0"/></pivotTableDefinition>"#
        );
    }

    #[test]
    fn refresh_flag_changes_only_the_cache_root() {
        let xml = r#"<?xml version="1.0"?><pivotCacheDefinition recordCount="3" refreshOnLoad="0"><cacheFields/></pivotCacheDefinition>"#;
        assert_eq!(
            refresh_on_load(xml).unwrap(),
            r#"<?xml version="1.0"?><pivotCacheDefinition recordCount="3" refreshOnLoad="1"><cacheFields/></pivotCacheDefinition>"#
        );
        assert_eq!(
            refresh_on_load("<other/>").unwrap_err(),
            "pivotCacheDefinition part has no root element."
        );
    }

    #[test]
    fn moved_origin_and_missing_sheet_are_rejected() {
        assert_eq!(
            validate_pivot_growth("A1:B2", "B1:C2", None).unwrap_err(),
            "Pivot \"B1:C2\" cannot change its top-left cell during refresh."
        );
        assert_eq!(
            validate_pivot_growth("A1:B2", "A1:C2", None).unwrap_err(),
            "Pivot expansion needs a resolved worksheet path."
        );
        assert!(validate_pivot_growth("A1:B2", "A1:A1", None).is_ok());
    }
}
