use super::super::{
    structure::{self, Axis, Shift, Span},
    xml,
};
use super::{Area, attributes, first, open_tag, parse_ref, replace_range, set};
use serde_json::Value;

#[derive(Clone, Debug)]
pub(in crate::host::d1) struct InsertedTableColumn {
    pub column: i64,
    pub id: u64,
    pub name: String,
}
#[derive(Clone, Debug)]
pub(in crate::host::d1) struct TableColumnInsertion {
    pub header_row: Option<i64>,
    pub columns: Vec<InsertedTableColumn>,
}
fn overlaps(a: Span, b: Span) -> bool {
    a.start <= b.end && b.start <= a.end
}
fn remap(records: &mut [TableColumnInsertion], axis: Axis, shift: Shift) {
    for record in records {
        match axis {
            Axis::Row => {
                if let Some(row) = record.header_row {
                    record.header_row = Some(structure::move_position(row, shift).unwrap_or(row));
                }
            }
            Axis::Column => {
                record
                    .columns
                    .retain_mut(|c| match structure::move_position(c.column, shift) {
                        Some(p) => {
                            c.column = p;
                            true
                        }
                        None => false,
                    });
            }
        }
    }
}
fn row_guard(name: &str, area: Area, header: i64, totals: i64, shift: Shift) -> Result<(), String> {
    let all = Span {
        start: area.r1,
        end: area.r2,
    };
    let head = Span {
        start: area.r1,
        end: area.r1 + header - 1,
    };
    let tail = Span {
        start: area.r2 - totals + 1,
        end: area.r2,
    };
    match shift {
        Shift::Delete { deleted } => {
            if header > 0 && overlaps(deleted, head) {
                return Err(format!(
                    "Deleting the header row of table \"{name}\" is not supported."
                ));
            }
            if totals > 0 && overlaps(deleted, tail) {
                return Err(format!(
                    "Deleting the totals row of table \"{name}\" is not supported."
                ));
            }
            let count = structure::move_range(all, shift)?
                .map_or(0, |s| s.end - s.start + 1 - header - totals);
            if count < 1 {
                return Err(format!(
                    "Deleting these rows would leave table \"{name}\" without data rows — not supported."
                ));
            }
        }
        Shift::Swap { first, second } => {
            if (all.start >= first.start && all.end <= first.end)
                || (all.start >= second.start && all.end <= second.end)
            {
                return Ok(());
            }
            let envelope = Span {
                start: first.start,
                end: second.end,
            };
            if header > 0 && overlaps(head, envelope) {
                return Err(format!(
                    "Moving the header row of table \"{name}\" is not supported."
                ));
            }
            if totals > 0 && overlaps(tail, envelope) {
                return Err(format!(
                    "Moving the totals row of table \"{name}\" is not supported."
                ));
            }
        }
        _ => {}
    }
    Ok(())
}
fn reshape(
    text: &str,
    name: &str,
    area: Area,
    header: i64,
    shift: Shift,
    records: &mut Vec<TableColumnInsertion>,
) -> Result<String, String> {
    let (first_index, last_index, insert) = match shift {
        Shift::Insert { boundary, count } if boundary > area.c1 && boundary <= area.c2 => (
            boundary - area.c1,
            boundary - area.c1,
            Some((boundary, count)),
        ),
        Shift::Delete { deleted }
            if overlaps(
                deleted,
                Span {
                    start: area.c1,
                    end: area.c2,
                },
            ) =>
        {
            if deleted.start <= area.c1 && deleted.end >= area.c2 {
                return Err(format!(
                    "Deleting all columns of table \"{name}\" is not supported — delete the table first."
                ));
            }
            (
                (deleted.start.max(area.c1) - area.c1),
                (deleted.end.min(area.c2) - area.c1),
                None,
            )
        }
        _ => return Ok(text.to_owned()),
    };
    let columns = xml::root_elements(text, "tableColumn")?;
    let width = area.c2 - area.c1 + 1;
    if columns.len() as i64 != width {
        return Err(format!(
            "Table \"{name}\" has {} column entries for {width} columns — aborted.",
            columns.len()
        ));
    }
    let block = first(text, "tableColumns")?.ok_or("Table has no tableColumns.")?;
    let mut parts = columns
        .iter()
        .map(|c| text[c.start..c.end].to_owned())
        .collect::<Vec<_>>();
    let (removed, count) = if let Some((boundary, count)) = insert {
        let mut taken = columns
            .iter()
            .filter_map(|c| c.attribute("name"))
            .map(str::to_lowercase)
            .collect::<std::collections::HashSet<_>>();
        let first_id = columns
            .iter()
            .filter_map(|c| c.attribute("id").and_then(|n| n.parse::<u64>().ok()))
            .max()
            .unwrap_or(0)
            + 1;
        let mut seed = 1u64;
        let mut added = Vec::new();
        let mut record = TableColumnInsertion {
            header_row: (header > 0).then_some(area.r1),
            columns: Vec::new(),
        };
        for (id, offset) in (first_id..).zip(0..count) {
            let name = loop {
                let n = format!("Column{seed}");
                if !taken.contains(&n.to_lowercase()) {
                    break n;
                }
                seed += 1;
            };
            taken.insert(name.to_lowercase());
            added.push(format!("<tableColumn id=\"{id}\" name=\"{name}\"/>"));
            record.columns.push(InsertedTableColumn {
                column: boundary + offset,
                id,
                name,
            });
        }
        parts.splice(first_index as usize..first_index as usize, added);
        records.push(record);
        (false, count)
    } else {
        let count = last_index - first_index + 1;
        parts.drain(first_index as usize..=last_index as usize);
        (true, count)
    };
    let mut attrs = attributes(&block);
    if block.attribute("count").is_some() {
        set(&mut attrs, "count", Some(parts.len().to_string()));
    }
    let result = format!(
        "{}{}</{}>",
        open_tag(&block.name, &attrs, false),
        parts.concat(),
        block.name
    );
    let mut xml_text = replace_range(text, block.start, block.end, &result);
    let filters = xml::root_elements(&xml_text, "filterColumn")?;
    for f in filters.iter().rev() {
        let Some(id) = f.attribute("colId").and_then(|s| s.parse::<i64>().ok()) else {
            continue;
        };
        if removed && id >= first_index && id <= last_index {
            xml_text = replace_range(&xml_text, f.start, f.end, "");
            continue;
        }
        let next = if removed && id > last_index {
            id - count
        } else if !removed && id >= first_index {
            id + count
        } else {
            id
        };
        if next != id {
            let mut a = attributes(f);
            set(&mut a, "colId", Some(next.to_string()));
            xml_text = replace_range(
                &xml_text,
                f.start,
                f.open_end,
                &open_tag(&f.name, &a, f.open_end == f.end),
            );
        }
    }
    if removed {
        for c in xml::root_elements(&xml_text, "sortCondition")?.iter().rev() {
            if c.attribute("ref")
                .and_then(|r| parse_ref(r).ok())
                .is_some_and(|a| a.c1 >= area.c1 + first_index && a.c2 <= area.c1 + last_index)
            {
                xml_text = replace_range(&xml_text, c.start, c.end, "");
            }
        }
        if let Some(s) = first(&xml_text, "sortState")?
            && s.open_end < s.close_start
            && xml_text[s.open_end..s.close_start].trim().is_empty()
        {
                xml_text = replace_range(&xml_text, s.start, s.end, "");
            }
    }
    Ok(xml_text)
}
fn shift_refs(text: &str, axis: Axis, shift: Shift) -> Result<String, String> {
    let mut items = Vec::new();
    for tag in ["table", "autoFilter", "sortState", "sortCondition"] {
        items.extend(xml::root_elements(text, tag)?);
    }
    items.sort_by_key(|e| e.start);
    let mut result = text.to_owned();
    for el in items.into_iter().rev() {
        let Some(reference) = el.attribute("ref") else {
            continue;
        };
        let Some(next) = structure::move_ref_range(reference, shift, axis)? else {
            continue;
        };
        let mut attrs = attributes(&el);
        set(&mut attrs, "ref", Some(next));
        result = replace_range(
            &result,
            el.start,
            el.open_end,
            &open_tag(&el.name, &attrs, el.open_end == el.end),
        );
    }
    Ok(result)
}
pub(in crate::host::d1) fn shift_table_part(
    text: &str,
    ops: &[Value],
) -> Result<(String, Vec<TableColumnInsertion>), String> {
    let mut result = text.to_owned();
    let mut records = Vec::new();
    for op in ops {
        if op.get("start").is_some() || op.get("summaryBelow").is_some() {
            continue;
        }
        let root =
            first(&result, "table")?.ok_or("An Excel table part has no readable ref — aborted.")?;
        let area = parse_ref(
            root.attribute("ref")
                .ok_or("An Excel table part has no readable ref — aborted.")?,
        )
        .map_err(|_| "An Excel table part has no readable ref — aborted.")?;
        let name = root.attribute("name").unwrap_or("Table").to_owned();
        let header = root
            .attribute("headerRowCount")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let totals = root
            .attribute("totalsRowCount")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if let Some(range) = op.get("range") {
            if op.get("kind").and_then(Value::as_str) == Some("merge-cells")
                && area.overlaps(Area::from_value(range)?)
            {
                return Err(format!(
                    "Merging cells over table \"{name}\" is not supported."
                ));
            }
            continue;
        }
        let Some((axis, shift)) = structure::parse_shift(op)? else {
            continue;
        };
        remap(&mut records, axis, shift);
        if axis == Axis::Row {
            row_guard(&name, area, header, totals, shift)?;
        } else {
            result = reshape(&result, &name, area, header, shift, &mut records)?;
        }
        result = shift_refs(&result, axis, shift)?;
    }
    Ok((result, records))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const TABLE: &str = "<table name=\"Sales\" ref=\"A1:B4\"><autoFilter ref=\"A1:B4\"><filterColumn colId=\"1\"><filters><filter val=\"x\"/></filters></filterColumn></autoFilter><tableColumns count=\"2\"><tableColumn id=\"1\" name=\"A\"/><tableColumn id=\"2\" name=\"B\"><calculatedColumnFormula>A2</calculatedColumnFormula></tableColumn></tableColumns></table>";
    #[test]
    fn insert_keeps_formula_and_moves_filter_and_header_record() {
        let (xml, records) =
            shift_table_part(TABLE, &[json!({"kind":"insert-cols","index":1,"count":1})]).unwrap();
        assert!(xml.contains("ref=\"A1:C4\""));
        assert!(xml.contains("<tableColumn id=\"3\" name=\"Column1\"/>"));
        assert!(xml.contains("<calculatedColumnFormula>A2</calculatedColumnFormula>"));
        assert!(xml.contains("colId=\"2\""));
        assert_eq!(records[0].header_row, Some(0));
        assert_eq!(records[0].columns[0].column, 1);
    }
    #[test]
    fn deleting_header_and_all_columns_fails_closed() {
        assert!(
            shift_table_part(TABLE, &[json!({"kind":"remove-rows","index":0,"count":1})])
                .unwrap_err()
                .contains("header")
        );
        assert!(
            shift_table_part(TABLE, &[json!({"kind":"remove-cols","index":0,"count":2})])
                .unwrap_err()
                .contains("all columns")
        );
    }
}
