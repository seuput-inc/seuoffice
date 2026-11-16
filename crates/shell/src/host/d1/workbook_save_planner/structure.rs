//! Coordinate transformations used by row and column structural edits.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Span {
    pub start: i64,
    pub end: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shift {
    Insert { boundary: i64, count: i64 },
    Delete { deleted: Span },
    Swap { first: Span, second: Span },
}

pub(super) fn move_position(position: i64, shift: Shift) -> Option<i64> {
    match shift {
        Shift::Insert { boundary, count } => Some(if position >= boundary {
            position + count
        } else {
            position
        }),
        Shift::Delete { deleted } => {
            if position >= deleted.start && position <= deleted.end {
                None
            } else if position > deleted.end {
                Some(position - (deleted.end - deleted.start + 1))
            } else {
                Some(position)
            }
        }
        Shift::Swap { first, second } => {
            if position >= first.start && position <= first.end {
                Some(position + second.end - second.start + 1)
            } else if position >= second.start && position <= second.end {
                Some(position - (first.end - first.start + 1))
            } else {
                Some(position)
            }
        }
    }
}

pub(super) fn move_range(range: Span, shift: Shift) -> Result<Option<Span>, String> {
    match shift {
        Shift::Insert { boundary, count } => Ok(Some(Span {
            start: if range.start >= boundary {
                range.start + count
            } else {
                range.start
            },
            end: if range.end >= boundary {
                range.end + count
            } else {
                range.end
            },
        })),
        Shift::Delete { deleted } => {
            if range.start >= deleted.start && range.end <= deleted.end {
                return Ok(None);
            }
            let count = deleted.end - deleted.start + 1;
            let start = if range.start > deleted.end {
                range.start - count
            } else if range.start >= deleted.start {
                deleted.start
            } else {
                range.start
            };
            let end = if range.end > deleted.end {
                range.end - count
            } else if range.end >= deleted.start {
                deleted.start - 1
            } else {
                range.end
            };
            Ok((end >= start).then_some(Span { start, end }))
        }
        Shift::Swap { first, second } => {
            if range.end < first.start
                || range.start > second.end
                || (range.start <= first.start && range.end >= second.end)
            {
                return Ok(Some(range));
            }
            if range.start >= first.start && range.end <= first.end {
                let delta = second.end - second.start + 1;
                return Ok(Some(Span {
                    start: range.start + delta,
                    end: range.end + delta,
                }));
            }
            if range.start >= second.start && range.end <= second.end {
                let delta = first.end - first.start + 1;
                return Ok(Some(Span {
                    start: range.start - delta,
                    end: range.end - delta,
                }));
            }
            Err("A range partially overlaps the moved rows — the move cannot be saved.".into())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Axis {
    Row,
    Column,
}

pub(super) fn parse_shift(op: &serde_json::Value) -> Result<Option<(Axis, Shift)>, String> {
    let Some(kind) = op.get("kind").and_then(serde_json::Value::as_str) else {
        return Err("Structural operation has no kind.".into());
    };
    let axis = match kind {
        "insert-rows" | "remove-rows" | "move-rows" => Axis::Row,
        "insert-cols" | "remove-cols" => Axis::Column,
        _ => return Ok(None),
    };
    let index = op
        .get("index")
        .and_then(serde_json::Value::as_i64)
        .ok_or("Invalid structural index.")?;
    let count = op
        .get("count")
        .and_then(serde_json::Value::as_i64)
        .ok_or("Invalid structural count.")?;
    let end = index
        .checked_add(count)
        .ok_or("Structural coordinate is too large.")?;
    let shift = match kind {
        "insert-rows" | "insert-cols" => Shift::Insert {
            boundary: index,
            count,
        },
        "remove-rows" | "remove-cols" => Shift::Delete {
            deleted: Span {
                start: index,
                end: end - 1,
            },
        },
        "move-rows" => {
            let before = op
                .get("before")
                .and_then(serde_json::Value::as_i64)
                .ok_or("Invalid row move target.")?;
            if before >= index && before <= end {
                return Err("A row move has a target inside the moved block — aborted.".into());
            }
            if before > index {
                Shift::Swap {
                    first: Span {
                        start: index,
                        end: end - 1,
                    },
                    second: Span {
                        start: end,
                        end: before - 1,
                    },
                }
            } else {
                Shift::Swap {
                    first: Span {
                        start: before,
                        end: index - 1,
                    },
                    second: Span {
                        start: index,
                        end: end - 1,
                    },
                }
            }
        }
        _ => unreachable!(),
    };
    Ok(Some((axis, shift)))
}

fn column_index(letters: &str) -> Option<i64> {
    if letters.is_empty() || letters.len() > 3 || !letters.bytes().all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    Some(
        letters
            .bytes()
            .fold(0i64, |sum, b| sum * 26 + i64::from(b - b'A' + 1))
            - 1,
    )
}
fn column_name(mut column: i64) -> String {
    let mut letters = Vec::new();
    column += 1;
    while column > 0 {
        let digit = u8::try_from((column - 1) % 26).unwrap_or(0);
        letters.push(char::from(b'A' + digit));
        column = (column - 1) / 26;
    }
    letters.into_iter().rev().collect()
}
struct CellReference {
    column: i64,
    row: i64,
    column_dollar: bool,
    row_dollar: bool,
}
fn parse_cell(token: &str) -> Option<CellReference> {
    let bytes = token.as_bytes();
    let mut at = 0;
    let column_dollar = bytes.first() == Some(&b'$');
    if column_dollar {
        at += 1;
    }
    let start = at;
    while bytes.get(at).is_some_and(u8::is_ascii_uppercase) {
        at += 1;
    }
    let column = column_index(&token[start..at])?;
    let row_dollar = bytes.get(at) == Some(&b'$');
    if row_dollar {
        at += 1;
    }
    let row = token.get(at..)?.parse::<i64>().ok()? - 1;
    Some(CellReference {
        column,
        row,
        column_dollar,
        row_dollar,
    })
}
fn render_cell(cell: &CellReference) -> String {
    format!(
        "{}{}{}{}",
        if cell.column_dollar { "$" } else { "" },
        column_name(cell.column),
        if cell.row_dollar { "$" } else { "" },
        cell.row + 1
    )
}
fn shift_reference(token: &str, shift: Shift, axis: Axis) -> Result<Option<String>, String> {
    let parts: Vec<_> = token.split(':').collect();
    if parts.len() == 2 {
        let first = parts[0].strip_prefix('$').unwrap_or(parts[0]);
        let second = parts[1].strip_prefix('$').unwrap_or(parts[1]);
        let dollars = (parts[0].starts_with('$'), parts[1].starts_with('$'));
        if let (Some(a), Some(b)) = (column_index(first), column_index(second)) {
            if axis == Axis::Row {
                return Ok(Some(token.to_owned()));
            }
            return Ok(move_range(Span { start: a, end: b }, shift)?.map(|range| {
                format!(
                    "{}{}:{}{}",
                    if dollars.0 { "$" } else { "" },
                    column_name(range.start),
                    if dollars.1 { "$" } else { "" },
                    column_name(range.end)
                )
            }));
        }
        if let (Ok(a), Ok(b)) = (first.parse::<i64>(), second.parse::<i64>()) {
            if axis == Axis::Column {
                return Ok(Some(token.to_owned()));
            }
            return Ok(move_range(
                Span {
                    start: a - 1,
                    end: b - 1,
                },
                shift,
            )?
            .map(|range| {
                format!(
                    "{}{}:{}{}",
                    if dollars.0 { "$" } else { "" },
                    range.start + 1,
                    if dollars.1 { "$" } else { "" },
                    range.end + 1
                )
            }));
        }
    }
    let Some(mut first) = parse_cell(parts[0]) else {
        return Ok(Some(token.to_owned()));
    };
    if parts.len() == 1 {
        let Some(position) = move_position(
            if axis == Axis::Row {
                first.row
            } else {
                first.column
            },
            shift,
        ) else {
            return Ok(None);
        };
        if axis == Axis::Row {
            first.row = position;
        } else {
            first.column = position;
        }
        return Ok(Some(render_cell(&first)));
    }
    let Some(mut second) = parse_cell(parts[1]) else {
        return Ok(Some(token.to_owned()));
    };
    let range = if axis == Axis::Row {
        Span {
            start: first.row,
            end: second.row,
        }
    } else {
        Span {
            start: first.column,
            end: second.column,
        }
    };
    let Some(moved) = move_range(range, shift)? else {
        return Ok(None);
    };
    if axis == Axis::Row {
        first.row = moved.start;
        second.row = moved.end;
    } else {
        first.column = moved.start;
        second.column = moved.end;
    }
    Ok(Some(format!(
        "{}:{}",
        render_cell(&first),
        render_cell(&second)
    )))
}

pub(super) fn move_ref_range(
    reference: &str,
    shift: Shift,
    axis: Axis,
) -> Result<Option<String>, String> {
    let mut parts = reference.split(':');
    let Some(mut first) = parts.next().and_then(parse_cell) else {
        return Ok(Some(reference.to_owned()));
    };
    first.row_dollar = false;
    first.column_dollar = false;
    let Some(second) = parts.next() else {
        let Some(position) = move_position(
            if axis == Axis::Row {
                first.row
            } else {
                first.column
            },
            shift,
        ) else {
            return Ok(None);
        };
        if axis == Axis::Row {
            first.row = position;
        } else {
            first.column = position;
        }
        return Ok(Some(render_cell(&first)));
    };
    let Some(mut second) = parse_cell(second) else {
        return Ok(Some(reference.to_owned()));
    };
    second.row_dollar = false;
    second.column_dollar = false;
    let range = if axis == Axis::Row {
        Span {
            start: first.row,
            end: second.row,
        }
    } else {
        Span {
            start: first.column,
            end: second.column,
        }
    };
    let Some(moved) = move_range(range, shift)? else {
        return Ok(None);
    };
    if axis == Axis::Row {
        first.row = moved.start;
        second.row = moved.end;
    } else {
        first.column = moved.start;
        second.column = moved.end;
    }
    Ok(Some(format!(
        "{}:{}",
        render_cell(&first),
        render_cell(&second)
    )))
}

pub(super) fn shift_formula_text(
    formula: &str,
    sheet_name: &str,
    shift: Shift,
    axis: Axis,
    qualified_only: bool,
) -> Result<String, String> {
    let regex =
        jscompat::TryJsRegex::new(super::sheets::REFERENCE, "gu").map_err(|e| e.to_string())?;
    let mut output = String::new();
    for (index, segment) in formula.split('"').enumerate() {
        if index != 0 {
            output.push('"');
        }
        if index % 2 == 1 {
            output.push_str(segment);
            continue;
        }
        let mut failure = None;
        let rewritten = regex
            .try_replace_all(segment, |matched| {
                let qualifier = matched.get(2);
                if qualifier.is_none() && qualified_only
                    || qualifier.is_some_and(|q| !super::sheets::qualifier_matches(q, sheet_name))
                {
                    return matched.as_str().to_owned();
                }
                let token = matched.get(3).unwrap_or("");
                let shifted = match shift_reference(token, shift, axis) {
                    Ok(Some(value)) => value,
                    Ok(None) => {
                        failure = Some(format!(
                            "A formula references the deleted range ({token}) — deletion aborted."
                        ));
                        return matched.as_str().to_owned();
                    }
                    Err(error) => {
                        failure = Some(error);
                        return matched.as_str().to_owned();
                    }
                };
                format!(
                    "{}{}{shifted}",
                    matched.get(1).unwrap_or(""),
                    qualifier.map(|q| format!("{q}!")).unwrap_or_default()
                )
            })
            .map_err(|e| e.to_string())?;
        if let Some(error) = failure {
            return Err(error);
        }
        output.push_str(&rewritten);
    }
    Ok(output)
}

struct AxisReference {
    shifts: bool,
    range: bool,
    start: i64,
    start_relative: bool,
    end: i64,
    end_relative: bool,
}
fn axis_ordinal(part: &str, axis: Axis) -> Option<(i64, bool)> {
    if let Some(cell) = parse_cell(part) {
        return Some(if axis == Axis::Row {
            (cell.row, !cell.row_dollar)
        } else {
            (cell.column, !cell.column_dollar)
        });
    }
    let (absolute, body) = match part.strip_prefix('$') {
        Some(body) => (true, body),
        None => (false, part),
    };
    if axis == Axis::Column {
        column_index(body).map(|column| (column, !absolute))
    } else {
        body.parse::<i64>().ok().map(|row| (row - 1, !absolute))
    }
}
fn axis_references(
    formula: &str,
    sheet: &str,
    axis: Axis,
    qualified_only: bool,
) -> Result<Vec<AxisReference>, String> {
    let regex =
        jscompat::TryJsRegex::new(super::sheets::REFERENCE, "gu").map_err(|e| e.to_string())?;
    let mut refs = Vec::new();
    for (index, segment) in formula.split('"').enumerate() {
        if index % 2 == 1 {
            continue;
        }
        for found in regex.try_find_iter(segment) {
            let found = found.map_err(|e| e.to_string())?;
            let parts: Vec<_> = found.get(3).unwrap_or("").split(':').collect();
            let Some((start, start_relative)) = parts.first().and_then(|p| axis_ordinal(p, axis))
            else {
                continue;
            };
            let Some((end, end_relative)) = parts.last().and_then(|p| axis_ordinal(p, axis)) else {
                continue;
            };
            let shifts = found
                .get(2)
                .map(|q| super::sheets::qualifier_matches(q, sheet))
                .unwrap_or(!qualified_only);
            refs.push(AxisReference {
                shifts,
                range: parts.len() > 1,
                start,
                start_relative,
                end,
                end_relative,
            });
        }
    }
    Ok(refs)
}
fn fragment_axis(fragment: &super::cells::SharedFragment<'_>, axis: Axis) -> Result<i64, String> {
    let coordinate = if axis == Axis::Row {
        fragment
            .row
            .checked_sub(1)
            .ok_or("Invalid shared formula row.")?
    } else {
        fragment.column
    };
    i64::try_from(coordinate).map_err(|_| "Shared formula coordinate is too large.".into())
}

pub(super) fn materialize_broken_shared_formulas(
    xml: &str,
    sheet: &str,
    shift: Shift,
    axis: Axis,
    cells_move: bool,
) -> Result<String, String> {
    if matches!(shift, Shift::Swap { .. }) {
        return Ok(xml.to_owned());
    }
    let fragments = super::cells::shared_formula_fragments(xml);
    let mut masters = std::collections::HashMap::new();
    for fragment in &fragments {
        if fragment.anchor && !fragment.body.is_empty() {
            masters.insert(fragment.si, fragment);
        }
    }
    let mut broken = std::collections::HashSet::new();
    for (si, master) in &masters {
        let position = fragment_axis(master, axis)?;
        let anchor_to = if cells_move {
            move_position(position, shift)
        } else {
            Some(position)
        };
        let Some(anchor_to) = anchor_to else {
            broken.insert(*si);
            continue;
        };
        let references = axis_references(
            &super::cells::decode_xml(master.body),
            sheet,
            axis,
            !cells_move,
        )?;
        let max = if axis == Axis::Row { 1_048_575 } else { 16_383 };
        for follower in fragments
            .iter()
            .filter(|fragment| fragment.si == *si && !fragment.anchor && fragment.body.is_empty())
        {
            let follower_position = fragment_axis(follower, axis)?;
            let target = if cells_move {
                move_position(follower_position, shift)
            } else {
                Some(follower_position)
            };
            let Some(target) = target else { continue };
            let offset = follower_position - position;
            let moved_offset = target - anchor_to;
            let mut safe = true;
            for reference in &references {
                let start = reference.start + if reference.start_relative { offset } else { 0 };
                let end = reference.end + if reference.end_relative { offset } else { 0 };
                if !(0..=max).contains(&start) || !(0..=max).contains(&end) {
                    safe = false;
                    break;
                }
                if !reference.range {
                    let before = if reference.shifts {
                        move_position(reference.start, shift)
                    } else {
                        Some(reference.start)
                    };
                    let after = if reference.shifts {
                        move_position(start, shift)
                    } else {
                        Some(start)
                    };
                    if before.is_none()
                        || after
                            != before.map(|p| {
                                p + if reference.start_relative {
                                    moved_offset
                                } else {
                                    0
                                }
                            })
                    {
                        safe = false;
                        break;
                    }
                } else {
                    let before = if reference.shifts {
                        move_range(
                            Span {
                                start: reference.start,
                                end: reference.end,
                            },
                            shift,
                        )?
                    } else {
                        Some(Span {
                            start: reference.start,
                            end: reference.end,
                        })
                    };
                    let after = if reference.shifts {
                        move_range(Span { start, end }, shift)?
                    } else {
                        Some(Span { start, end })
                    };
                    let expected = before.map(|p| Span {
                        start: p.start
                            + if reference.start_relative {
                                moved_offset
                            } else {
                                0
                            },
                        end: p.end
                            + if reference.end_relative {
                                moved_offset
                            } else {
                                0
                            },
                    });
                    if before.is_none() || after != expected {
                        safe = false;
                        break;
                    }
                }
            }
            if !safe {
                broken.insert(*si);
                break;
            }
        }
    }
    if broken.is_empty() {
        return Ok(xml.to_owned());
    }
    let mut output = String::new();
    let mut cursor = 0;
    for fragment in &fragments {
        if !broken.contains(fragment.si) {
            continue;
        }
        let Some(master) = masters.get(fragment.si) else {
            continue;
        };
        if cells_move && move_position(fragment_axis(fragment, axis)?, shift).is_none() {
            continue;
        }
        if !fragment.anchor && !fragment.body.is_empty() {
            continue;
        }
        let body = if fragment.anchor {
            fragment.body.to_owned()
        } else {
            let row_delta = i64::try_from(fragment.row)
                .map_err(|_| "Shared formula row is too large.")?
                - i64::try_from(master.row).map_err(|_| "Shared formula row is too large.")?;
            let col_delta = i64::try_from(fragment.column)
                .map_err(|_| "Shared formula column is too large.")?
                - i64::try_from(master.column)
                    .map_err(|_| "Shared formula column is too large.")?;
            let column = i64::try_from(fragment.column)
                .map_err(|_| "Shared formula column is too large.")?;
            let translated = sheet_native::translate_shared_formula(
                &super::cells::decode_xml(master.body),
                row_delta,
                col_delta,
            )
            .ok_or_else(|| {
                format!(
                    "The shared formula in {}{} would reference cells outside the sheet — aborted.",
                    column_name(column),
                    fragment.row
                )
            })?;
            super::cells::xml_text(&translated)
        };
        let mut open =
            fragment.xml[..fragment.xml.find('>').unwrap_or(fragment.xml.len()) + 1].to_owned();
        for key in ["t", "ref", "si"] {
            open = super::sheets::set_attribute(&open, key, None);
        }
        if open.ends_with("/>") {
            open.truncate(open.len() - 2);
            open.push('>');
        }
        output.push_str(&xml[cursor..fragment.start]);
        output.push_str(&format!("{open}{body}</f>"));
        cursor = fragment.end;
    }
    output.push_str(&xml[cursor..]);
    Ok(output)
}

#[derive(Clone, Copy)]
pub(super) enum FormulaPart {
    Worksheet,
    Workbook,
    Chart,
}

pub(super) fn shift_qualified_formulas(
    xml: &str,
    sheet: &str,
    shift: Shift,
    axis: Axis,
    part: FormulaPart,
) -> Result<String, String> {
    let mut elements = Vec::new();
    super::sheets::descendants(xml, &mut elements);
    let mut changes = Vec::new();
    for element in elements {
        let is_formula = match part {
            FormulaPart::Worksheet => {
                matches!(element.name, "f" | "formula" | "formula1" | "formula2")
            }
            FormulaPart::Workbook => element.name == "definedName",
            FormulaPart::Chart => element.name == "c:f",
        };
        if is_formula && !element.inner.is_empty() {
            let decoded = element
                .inner
                .replace("&quot;", "\"")
                .replace("&apos;", "'")
                .replace("&gt;", ">")
                .replace("&lt;", "<")
                .replace("&amp;", "&");
            let shifted = shift_formula_text(&decoded, sheet, shift, axis, true)?;
            let start = element.inner.as_ptr() as usize - xml.as_ptr() as usize;
            changes.push((
                start,
                start + element.inner.len(),
                super::cells::xml_text(&shifted),
            ));
        }
        if matches!(part, FormulaPart::Worksheet)
            && element.name == "hyperlink"
            && let Some(location) = super::styles::attr(element.xml, "location")
        {
                let decoded = location
                    .replace("&quot;", "\"")
                    .replace("&apos;", "'")
                    .replace("&gt;", ">")
                    .replace("&lt;", "<")
                    .replace("&amp;", "&");
                let shifted = shift_formula_text(&decoded, sheet, shift, axis, true)?;
                let start = element.xml.as_ptr() as usize - xml.as_ptr() as usize;
                changes.push((
                    start,
                    start + element.xml.len(),
                    super::sheets::set_attribute(element.xml, "location", Some(&shifted)),
                ));
            }
    }
    let mut result = xml.to_owned();
    changes.sort_unstable_by_key(|(start, _, _)| *start);
    for (start, end, replacement) in changes.into_iter().rev() {
        result.replace_range(start..end, &replacement);
    }
    Ok(result)
}

pub(super) fn shift_sheet_formulas(
    xml: &str,
    sheet: &str,
    shift: Shift,
    axis: Axis,
) -> Result<String, String> {
    let mut elements = Vec::new();
    super::sheets::descendants(xml, &mut elements);
    let mut changes = Vec::new();
    for element in elements {
        if !matches!(element.name, "f" | "formula" | "formula1" | "formula2")
            || element.xml.ends_with("/>")
        {
            continue;
        }
        let decoded = element
            .inner
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&gt;", ">")
            .replace("&lt;", "<")
            .replace("&amp;", "&");
        let shifted = shift_formula_text(&decoded, sheet, shift, axis, false)?;
        let open_end = element.xml.find('>').ok_or("Malformed formula element.")? + 1;
        let mut open = element.xml[..open_end].to_owned();
        if element.name == "f"
            && let Some(reference) = super::styles::attr(element.xml, "ref")
        {
                if let Shift::Swap { first, second } = shift
                    && axis == Axis::Row
                {
                        let mut refs = reference.split(':');
                        if let (Some(start), Some(end)) = (
                            refs.next().and_then(parse_cell),
                            reference.rsplit(':').next().and_then(parse_cell),
                        ) {
                            let outside = end.row < first.start || start.row > second.end;
                            let in_first = start.row >= first.start && end.row <= first.end;
                            let in_second = start.row >= second.start && end.row <= second.end;
                            if !outside && !in_first && !in_second {
                                return Err("A shared formula anchor overlaps the moved rows — the move cannot be saved.".into());
                            }
                        }
                    }
                let moved = move_ref_range(reference, shift, axis)?
                    .ok_or("Deletion would remove a shared formula anchor — aborted.")?;
                open = super::sheets::set_attribute(&open, "ref", Some(&moved));
            }
        let closing = format!("</{}>", element.name);
        let rewritten = format!("{}{}{}", open, super::cells::xml_text(&shifted), closing);
        let start = element.xml.as_ptr() as usize - xml.as_ptr() as usize;
        changes.push((start, start + element.xml.len(), rewritten));
    }
    let mut result = xml.to_owned();
    for (start, end, replacement) in changes.into_iter().rev() {
        result.replace_range(start..end, &replacement);
    }
    Ok(result)
}

fn shift_row_xml(
    row: &str,
    position: i64,
    axis: Axis,
    shift: Shift,
) -> Result<Option<(i64, String)>, String> {
    let root = super::style_entries::children(row)
        .into_iter()
        .next()
        .ok_or("Malformed worksheet row.")?;
    let row_position = if axis == Axis::Row {
        let Some(moved) = move_position(position, shift) else {
            return Ok(None);
        };
        moved
    } else {
        position
    };
    let mut changes = Vec::new();
    for cell in super::style_entries::children(root.inner)
        .into_iter()
        .filter(|e| e.name == "c")
    {
        let Some(reference) = super::styles::attr(cell.xml, "r") else {
            continue;
        };
        let Some(mut parsed) = parse_cell(reference) else {
            continue;
        };
        let next = if axis == Axis::Row {
            Some(row_position)
        } else {
            move_position(parsed.column, shift)
        };
        let at = cell.xml.as_ptr() as usize - row.as_ptr() as usize;
        if let Some(next) = next {
            if axis == Axis::Row {
                parsed.row = next;
            } else {
                parsed.column = next;
            }
            let value = super::sheets::set_attribute(cell.xml, "r", Some(&render_cell(&parsed)));
            changes.push((at, at + cell.xml.len(), value));
        } else {
            changes.push((at, at + cell.xml.len(), String::new()));
        }
    }
    let mut updated = row.to_owned();
    for (start, end, value) in changes.into_iter().rev() {
        updated.replace_range(start..end, &value);
    }
    if axis == Axis::Row && row_position != position {
        updated =
            super::sheets::set_attribute(&updated, "r", Some(&(row_position + 1).to_string()));
    }
    if axis == Axis::Column {
        updated = super::sheets::set_attribute(&updated, "spans", None);
    }
    Ok(Some((row_position, updated)))
}

fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}
fn anchor_mark(position: i64, shift: Shift) -> (i64, bool) {
    match shift {
        Shift::Delete { deleted } if position >= deleted.start && position <= deleted.end => {
            (deleted.start, true)
        }
        _ => (move_position(position, shift).unwrap_or(position), false),
    }
}
fn anchor_child<'a>(
    anchor: super::style_entries::Element<'a>,
    name: &str,
) -> Option<super::style_entries::Element<'a>> {
    let mut nodes = Vec::new();
    super::sheets::descendants(anchor.inner, &mut nodes);
    nodes.into_iter().find(|node| local(node.name) == name)
}
fn anchor_value<'a>(
    mark: super::style_entries::Element<'a>,
    tag: &str,
) -> Option<(super::style_entries::Element<'a>, i64)> {
    let child = anchor_child(mark, tag)?;
    Some((child, child.inner.parse().ok()?))
}

pub(super) fn shift_drawing_anchors(
    xml: &str,
    ops: &[serde_json::Value],
) -> Result<String, String> {
    let mut result = xml.to_owned();
    for op in ops {
        let Some((axis, shift)) = parse_shift(op)? else {
            continue;
        };
        let tag = if axis == Axis::Row { "row" } else { "col" };
        let offset_tag = if axis == Axis::Row {
            "rowOff"
        } else {
            "colOff"
        };
        let mut nodes = Vec::new();
        super::sheets::descendants(&result, &mut nodes);
        let mut changes = Vec::new();
        if matches!(shift, Shift::Swap { .. }) {
            for anchor in nodes.iter().copied().filter(|node| {
                matches!(
                    local(node.name),
                    "twoCellAnchor" | "oneCellAnchor" | "anchor"
                )
            }) {
                let Some(from) =
                    anchor_child(anchor, "from").and_then(|mark| anchor_value(mark, tag))
                else {
                    continue;
                };
                let to = anchor_child(anchor, "to").and_then(|mark| anchor_value(mark, tag));
                if local(anchor.name) == "oneCellAnchor" {
                    if let Some(moved) = move_position(from.1, shift)
                        && moved != from.1
                    {
                            let start = from.0.inner.as_ptr() as usize - result.as_ptr() as usize;
                            changes.push((start, start + from.0.inner.len(), moved.to_string()));
                        }
                } else if let Some(to) = to
                    && let Some(moved) = move_range(
                        Span {
                            start: from.1,
                            end: to.1,
                        },
                        shift,
                    )?
                    && (moved.start != from.1 || moved.end != to.1)
                {
                            for (element, value) in [(from.0, moved.start), (to.0, moved.end)] {
                                let start =
                                    element.inner.as_ptr() as usize - result.as_ptr() as usize;
                                changes.push((
                                    start,
                                    start + element.inner.len(),
                                    value.to_string(),
                                ));
                            }
                }
            }
        } else {
            for mark in nodes
                .iter()
                .copied()
                .filter(|node| matches!(local(node.name), "from" | "to"))
            {
                let Some((element, value)) = anchor_value(mark, tag) else {
                    continue;
                };
                let (moved, clamped) = anchor_mark(value, shift);
                let start = element.inner.as_ptr() as usize - result.as_ptr() as usize;
                changes.push((start, start + element.inner.len(), moved.to_string()));
                if clamped
                    && let Some(offset) = anchor_child(mark, offset_tag)
                    && offset.inner.parse::<i64>().is_ok()
                {
                            let start = offset.inner.as_ptr() as usize - result.as_ptr() as usize;
                            changes.push((start, start + offset.inner.len(), "0".into()));
                        }
            }
        }
        changes.sort_unstable_by_key(|(start, _, _)| *start);
        for (start, end, value) in changes.into_iter().rev() {
            result.replace_range(start..end, &value);
        }
    }
    Ok(result)
}

pub(super) fn shift_ole_anchors(xml: &str, ops: &[serde_json::Value]) -> Result<String, String> {
    if !xml.contains("<oleObjects") {
        return Ok(xml.to_owned());
    }
    replace_xml_elements(xml, "oleObjects", |element| {
        shift_drawing_anchors(element.xml, ops)
    })
}

pub(super) fn shift_sheet_positions(xml: &str, shift: Shift, axis: Axis) -> Result<String, String> {
    let root = super::style_entries::children(xml)
        .into_iter()
        .next()
        .ok_or("Worksheet has no sheetData element.")?;
    let Some(data) = super::style_entries::children(root.inner)
        .into_iter()
        .find(|e| e.name == "sheetData")
    else {
        return Ok(xml.to_owned());
    };
    let mut inner = String::new();
    let mut cursor = 0;
    let mut sorted = Vec::new();
    let swap = matches!(shift, Shift::Swap { .. });
    for row in super::style_entries::children(data.inner)
        .into_iter()
        .filter(|e| e.name == "row")
    {
        let start = row.xml.as_ptr() as usize - data.inner.as_ptr() as usize;
        let gap = &data.inner[cursor..start];
        if swap && !gap.trim().is_empty() {
            return Err("sheetData holds content other than rows — move aborted.".into());
        }
        if !swap {
            inner.push_str(gap);
        }
        if let Some(index) =
            super::styles::attr(row.xml, "r").and_then(|value| value.parse::<i64>().ok())
        {
            if let Some((position, text)) = shift_row_xml(row.xml, index - 1, axis, shift)? {
                if swap {
                    sorted.push((position, text));
                } else {
                    inner.push_str(&text);
                }
            }
        } else if swap {
            return Err("sheetData holds content other than rows — move aborted.".into());
        } else {
            inner.push_str(row.xml);
        }
        cursor = start + row.xml.len();
    }
    let remainder = &data.inner[cursor..];
    if swap {
        if !remainder.trim().is_empty() {
            return Err("sheetData holds content other than rows — move aborted.".into());
        }
        sorted.sort_by_key(|(position, _)| *position);
        for (_, row) in sorted {
            inner.push_str(&row);
        }
    } else {
        inner.push_str(remainder);
    }
    if data.xml.ends_with("/>") {
        return Ok(xml.to_owned());
    }
    let start = data.inner.as_ptr() as usize - xml.as_ptr() as usize;
    Ok(format!(
        "{}{}{}",
        &xml[..start],
        inner,
        &xml[start + data.inner.len()..]
    ))
}

fn replace_xml_elements<F>(xml: &str, tag: &str, mut edit: F) -> Result<String, String>
where
    F: FnMut(super::style_entries::Element<'_>) -> Result<String, String>,
{
    let mut elements = Vec::new();
    super::sheets::descendants(xml, &mut elements);
    let mut changes = Vec::new();
    for element in elements.into_iter().filter(|e| e.name == tag) {
        let at = element.xml.as_ptr() as usize - xml.as_ptr() as usize;
        changes.push((at, at + element.xml.len(), edit(element)?));
    }
    let mut result = xml.to_owned();
    for (start, end, replacement) in changes.into_iter().rev() {
        result.replace_range(start..end, &replacement);
    }
    Ok(result)
}

pub(super) fn shift_column_definitions(xml: &str, shift: Shift) -> Result<String, String> {
    replace_xml_elements(xml, "col", |element| {
        if !element.xml.ends_with("/>") {
            return Ok(element.xml.to_owned());
        }
        let start = super::styles::attr(element.xml, "min")
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|v| *v > 0);
        let end = super::styles::attr(element.xml, "max")
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|v| *v > 0);
        let (Some(start), Some(end)) = (start, end) else {
            return Ok(element.xml.to_owned());
        };
        let Some(moved) = move_range(
            Span {
                start: start - 1,
                end: end - 1,
            },
            shift,
        )?
        else {
            return Ok(String::new());
        };
        let changed =
            super::sheets::set_attribute(element.xml, "min", Some(&(moved.start + 1).to_string()));
        Ok(super::sheets::set_attribute(
            &changed,
            "max",
            Some(&(moved.end + 1).to_string()),
        ))
    })
}

fn shift_auto_filter(xml: &str, shift: Shift, axis: Axis) -> Result<String, String> {
    let Some(reference) = super::styles::attr(xml, "ref") else {
        return Ok(xml.to_owned());
    };
    let Some(moved) = move_ref_range(reference, shift, axis)? else {
        return Ok(xml.to_owned());
    };
    let from = reference
        .split(':')
        .next()
        .and_then(parse_cell)
        .map(|c| c.column);
    let to = moved
        .split(':')
        .next()
        .and_then(parse_cell)
        .map(|c| c.column);
    let mut result = super::sheets::set_attribute(xml, "ref", Some(&moved));
    if let (Some(from), Some(to)) = (from, to) {
        result = replace_xml_elements(&result, "filterColumn", |element| {
            if axis != Axis::Column {
                return Ok(element.xml.to_owned());
            }
            let Some(column) =
                super::styles::attr(element.xml, "colId").and_then(|s| s.parse::<i64>().ok())
            else {
                return Ok(element.xml.to_owned());
            };
            let Some(next) = move_position(from + column, shift) else {
                return Ok(String::new());
            };
            Ok(super::sheets::set_attribute(
                element.xml,
                "colId",
                Some(&(next - to).to_string()),
            ))
        })?;
    }
    result = replace_xml_elements(&result, "sortCondition", |element| {
        let Some(reference) = super::styles::attr(element.xml, "ref") else {
            return Ok(element.xml.to_owned());
        };
        Ok(match move_ref_range(reference, shift, axis)? {
            Some(moved) => super::sheets::set_attribute(element.xml, "ref", Some(&moved)),
            None => String::new(),
        })
    })?;
    replace_xml_elements(&result, "sortState", |element| {
        let Some(reference) = super::styles::attr(element.xml, "ref") else {
            return Ok(element.xml.to_owned());
        };
        let moved = move_ref_range(reference, shift, axis)?;
        let result = moved
            .map(|value| super::sheets::set_attribute(element.xml, "ref", Some(&value)))
            .unwrap_or_else(|| element.xml.to_owned());
        let inner = super::style_entries::children(&result)
            .into_iter()
            .next()
            .map(|e| e.inner.trim().is_empty() && !e.xml.ends_with("/>"))
            .unwrap_or(false);
        Ok(if inner { String::new() } else { result })
    })
}

pub(super) fn shift_ranged_features(xml: &str, shift: Shift, axis: Axis) -> Result<String, String> {
    let mut result = replace_xml_elements(xml, "mergeCell", |element| {
        let Some(reference) = super::styles::attr(element.xml, "ref") else {
            return Ok(element.xml.to_owned());
        };
        Ok(match move_ref_range(reference, shift, axis)? {
            Some(moved) => super::sheets::set_attribute(element.xml, "ref", Some(&moved)),
            None => String::new(),
        })
    })?;
    result = replace_xml_elements(&result, "mergeCells", |element| {
        let count = super::style_entries::children(element.inner)
            .iter()
            .filter(|child| child.name == "mergeCell")
            .count();
        if count == 0 {
            return Ok(String::new());
        }
        if super::styles::attr(element.xml, "count").is_some() {
            Ok(super::sheets::set_attribute(
                element.xml,
                "count",
                Some(&count.to_string()),
            ))
        } else {
            Ok(element.xml.to_owned())
        }
    })?;
    result = replace_xml_elements(&result, "dimension", |element| {
        let Some(reference) = super::styles::attr(element.xml, "ref") else {
            return Ok(element.xml.to_owned());
        };
        Ok(move_ref_range(reference, shift, axis)?
            .map(|moved| super::sheets::set_attribute(element.xml, "ref", Some(&moved)))
            .unwrap_or_else(|| element.xml.to_owned()))
    })?;
    result = replace_xml_elements(&result, "autoFilter", |element| {
        shift_auto_filter(element.xml, shift, axis)
    })?;
    for tag in ["hyperlink", "dataValidation", "conditionalFormatting"] {
        let key = if tag == "hyperlink" { "ref" } else { "sqref" };
        result = replace_xml_elements(&result, tag, |element| {
            let Some(reference) = super::styles::attr(element.xml, key) else {
                return Ok(element.xml.to_owned());
            };
            let mut moved = Vec::new();
            for item in reference.split(' ') {
                if let Some(value) = move_ref_range(item, shift, axis)? {
                    moved.push(value);
                }
            }
            if moved.is_empty() {
                Ok(String::new())
            } else {
                Ok(super::sheets::set_attribute(
                    element.xml,
                    key,
                    Some(&moved.join(" ")),
                ))
            }
        })?;
    }
    replace_xml_elements(&result, "dataValidations", |element| {
        if super::styles::attr(element.xml, "count").is_none() || element.xml.ends_with("/>") {
            return Ok(element.xml.to_owned());
        }
        let count = super::style_entries::children(element.inner)
            .iter()
            .filter(|child| child.name == "dataValidation")
            .count();
        if count == 0 {
            Ok(String::new())
        } else {
            Ok(super::sheets::set_attribute(
                element.xml,
                "count",
                Some(&count.to_string()),
            ))
        }
    })
}

fn append_attribute(xml: &str, name: &str, value: &str) -> String {
    let Some(end) = xml.find('>') else {
        return xml.to_owned();
    };
    let position = if xml.as_bytes().get(end.wrapping_sub(1)) == Some(&b'/') {
        end - 1
    } else {
        end
    };
    format!(
        "{} {name}=\"{}\"{}",
        &xml[..position],
        super::styles::esc(value),
        &xml[position..]
    )
}
fn patch_row_attributes(row: &str, op: &serde_json::Value) -> Result<String, String> {
    let mut result = row.to_owned();
    if let Some(size) = op.get("size") {
        result = super::sheets::set_attribute(&result, "ht", None);
        result = super::sheets::set_attribute(&result, "customHeight", None);
        if !size.is_null() {
            let number = size.as_f64().ok_or("Invalid row height.")?;
            let formatted =
                jscompat::number_to_string(jscompat::math_round(number * 100.0) / 100.0);
            result = append_attribute(&result, "ht", &formatted);
            result = append_attribute(&result, "customHeight", "1");
        }
    } else if let Some(level) = op.get("level") {
        result = super::sheets::set_attribute(&result, "outlineLevel", None);
        let level = level.as_u64().ok_or("Invalid row outline level.")?;
        if level > 0 {
            result = append_attribute(&result, "outlineLevel", &level.to_string());
        }
        if let Some(collapsed) = op.get("collapsed") {
            result = super::sheets::set_attribute(&result, "collapsed", None);
            if collapsed.as_bool() == Some(true) {
                result = append_attribute(&result, "collapsed", "1");
            }
        }
    } else {
        result = super::sheets::set_attribute(&result, "hidden", None);
        if op.get("hidden").and_then(serde_json::Value::as_bool) == Some(true) {
            result = append_attribute(&result, "hidden", "1");
        }
    }
    Ok(result)
}

pub(super) fn apply_row_attributes(xml: &str, op: &serde_json::Value) -> Result<String, String> {
    let start = super::cells::coordinate(op, "start")?;
    let end = super::cells::coordinate(op, "end")?.min(1_048_575);
    if start > end {
        return Ok(xml.to_owned());
    }
    let sets = if let Some(size) = op.get("size") {
        !size.is_null()
    } else if let Some(level) = op.get("level") {
        level.as_u64().unwrap_or(0) > 0
            || op.get("collapsed").and_then(serde_json::Value::as_bool) == Some(true)
    } else {
        op.get("hidden").and_then(serde_json::Value::as_bool) == Some(true)
    };
    let root = super::style_entries::children(xml)
        .into_iter()
        .next()
        .ok_or("Worksheet has no sheetData element.")?;
    let data = super::style_entries::children(root.inner)
        .into_iter()
        .find(|e| e.name == "sheetData")
        .ok_or("Worksheet has no sheetData element.")?;
    let rows = super::style_entries::children(data.inner);
    let mut seen = std::collections::HashSet::new();
    for row in rows.iter().filter(|row| row.name == "row") {
        if let Some(index) = super::styles::attr(row.xml, "r")
            .and_then(|value| value.parse::<usize>().ok())
            .and_then(|n| n.checked_sub(1))
            && (start..=end).contains(&index)
        {
                seen.insert(index);
            }
    }
    let mut missing = (start..=end)
        .filter(|index| sets && !seen.contains(index))
        .peekable();
    let mut body = String::new();
    let mut cursor = 0;
    for row in rows.into_iter().filter(|row| row.name == "row") {
        let offset = row.xml.as_ptr() as usize - data.inner.as_ptr() as usize;
        body.push_str(&data.inner[cursor..offset]);
        let index = super::styles::attr(row.xml, "r")
            .and_then(|value| value.parse::<usize>().ok())
            .and_then(|n| n.checked_sub(1));
        while missing
            .peek()
            .is_some_and(|candidate| index.is_some_and(|value| *candidate < value))
        {
            if let Some(next) = missing.next() {
                body.push_str(&patch_row_attributes(
                    &format!("<row r=\"{}\"/>", next + 1),
                    op,
                )?);
            }
        }
        if index.is_some_and(|index| (start..=end).contains(&index)) {
            body.push_str(&patch_row_attributes(row.xml, op)?);
        } else {
            body.push_str(row.xml);
        }
        cursor = offset + row.xml.len();
    }
    body.push_str(&data.inner[cursor..]);
    for next in missing {
        body.push_str(&patch_row_attributes(
            &format!("<row r=\"{}\"/>", next + 1),
            op,
        )?);
    }
    let at = data.xml.as_ptr() as usize - xml.as_ptr() as usize;
    let open_end = data
        .xml
        .find('>')
        .ok_or("Worksheet has no sheetData element.")?
        + 1;
    let open = if data.xml[..open_end].ends_with("/>") {
        format!("{}>", &data.xml[..open_end - 2])
    } else {
        data.xml[..open_end].to_owned()
    };
    if data.xml.ends_with("/>") && body.is_empty() {
        return Ok(xml.to_owned());
    }
    Ok(format!(
        "{}{}{}{}</sheetData>{}",
        &xml[..at],
        open,
        body,
        "",
        &xml[at + data.xml.len()..]
    ))
}

#[derive(Clone)]
struct ColDefinition {
    min: i64,
    max: i64,
    attrs: Vec<(String, String)>,
}
fn set_col_attr(attrs: &mut Vec<(String, String)>, name: &str, value: Option<String>) {
    if let Some(at) = attrs.iter().position(|(key, _)| key == name) {
        if let Some(value) = value {
            attrs[at].1 = value;
        } else {
            attrs.remove(at);
        }
    } else if let Some(value) = value {
        attrs.push((name.to_owned(), value));
    }
}
fn patch_col_attrs(
    attrs: &mut Vec<(String, String)>,
    op: &serde_json::Value,
    stylesheet: &mut Option<&mut super::styles::StylesheetEditor>,
) -> Result<(), String> {
    if let Some(style) = op.get("style") {
        let editor = stylesheet
            .as_deref_mut()
            .ok_or("Column style edit has no stylesheet.")?;
        let base = attrs
            .iter()
            .find(|(name, _)| name == "style")
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        set_col_attr(
            attrs,
            "style",
            Some(editor.resolve_style(base, style)?.to_string()),
        );
    } else if let Some(size) = op.get("size") {
        for name in ["width", "customWidth", "bestFit"] {
            set_col_attr(attrs, name, None);
        }
        if !size.is_null() {
            let value = size.as_f64().ok_or("Invalid column width.")?;
            set_col_attr(attrs, "width", Some(jscompat::number_to_string(value)));
            set_col_attr(attrs, "customWidth", Some("1".into()));
        }
    } else if let Some(level) = op.get("level") {
        set_col_attr(attrs, "outlineLevel", None);
        let level = level.as_u64().ok_or("Invalid column outline level.")?;
        if level > 0 {
            set_col_attr(attrs, "outlineLevel", Some(level.to_string()));
        }
        if let Some(collapsed) = op.get("collapsed") {
            set_col_attr(attrs, "collapsed", None);
            if collapsed.as_bool() == Some(true) {
                set_col_attr(attrs, "collapsed", Some("1".into()));
            }
        }
    } else {
        set_col_attr(attrs, "hidden", None);
        if op.get("hidden").and_then(serde_json::Value::as_bool) == Some(true) {
            set_col_attr(attrs, "hidden", Some("1".into()));
        }
    }
    Ok(())
}

pub(super) fn apply_col_attributes(
    xml: &str,
    op: &serde_json::Value,
    mut stylesheet: Option<&mut super::styles::StylesheetEditor>,
) -> Result<String, String> {
    let start = op
        .get("start")
        .and_then(serde_json::Value::as_i64)
        .ok_or("Invalid column start.")?;
    let end = op
        .get("end")
        .and_then(serde_json::Value::as_i64)
        .ok_or("Invalid column end.")?;
    let root = super::style_entries::children(xml)
        .into_iter()
        .next()
        .ok_or("Worksheet has no sheetData element.")?;
    let root_children = super::style_entries::children(root.inner);
    let section = root_children
        .iter()
        .find(|element| element.name == "cols" && !element.xml.ends_with("/>"));
    let re =
        jscompat::TryJsRegex::new(r#"([\w:]+)=\"([^\"]*)\""#, "g").map_err(|e| e.to_string())?;
    let mut existing = Vec::new();
    if let Some(section) = section {
        for element in super::style_entries::children(section.inner)
            .into_iter()
            .filter(|element| element.name == "col" && element.xml.ends_with("/>"))
        {
            let mut attrs = Vec::new();
            for hit in re.try_find_iter(element.xml) {
                let hit = hit.map_err(|e| e.to_string())?;
                if let (Some(name), Some(value)) = (hit.get(1), hit.get(2)) {
                    set_col_attr(&mut attrs, name, Some(value.into()));
                }
            }
            let min = attrs
                .iter()
                .find(|(key, _)| key == "min")
                .and_then(|(_, value)| value.parse::<i64>().ok());
            let max = attrs
                .iter()
                .find(|(key, _)| key == "max")
                .and_then(|(_, value)| value.parse::<i64>().ok());
            if let (Some(min), Some(max)) = (min, max) {
                set_col_attr(&mut attrs, "min", None);
                set_col_attr(&mut attrs, "max", None);
                existing.push(ColDefinition {
                    min: min - 1,
                    max: max - 1,
                    attrs,
                });
            }
        }
    }
    let mut result = Vec::new();
    for col in existing {
        if col.max < start || col.min > end {
            result.push(col);
            continue;
        }
        if col.min < start {
            result.push(ColDefinition {
                min: col.min,
                max: start - 1,
                attrs: col.attrs.clone(),
            });
        }
        let mut middle = col.attrs.clone();
        patch_col_attrs(&mut middle, op, &mut stylesheet)?;
        result.push(ColDefinition {
            min: col.min.max(start),
            max: col.max.min(end),
            attrs: middle,
        });
        if col.max > end {
            result.push(ColDefinition {
                min: end + 1,
                max: col.max,
                attrs: col.attrs,
            });
        }
    }
    let sets = if op.get("style").is_some() {
        true
    } else if let Some(size) = op.get("size") {
        !size.is_null()
    } else if let Some(level) = op.get("level") {
        level.as_u64().unwrap_or(0) > 0
            || op.get("collapsed").and_then(serde_json::Value::as_bool) == Some(true)
    } else {
        op.get("hidden").and_then(serde_json::Value::as_bool) == Some(true)
    };
    if sets {
        let mut spans: Vec<_> = result
            .iter()
            .filter(|col| col.max >= start && col.min <= end)
            .map(|col| (col.min, col.max))
            .collect();
        spans.sort_by_key(|(min, _)| *min);
        let mut cursor = start;
        let mut gaps = Vec::new();
        for (min, max) in spans {
            if min > cursor {
                gaps.push((cursor, min - 1));
            }
            cursor = cursor.max(max + 1);
        }
        if cursor <= end {
            gaps.push((cursor, end));
        }
        for (min, max) in gaps {
            let mut attrs = Vec::new();
            patch_col_attrs(&mut attrs, op, &mut stylesheet)?;
            result.push(ColDefinition { min, max, attrs });
        }
    }
    result.retain(|col| !col.attrs.is_empty());
    result.sort_by_key(|col| col.min);
    let mut content = String::new();
    for col in &result {
        content.push_str(&format!(
            "<col min=\"{}\" max=\"{}\"",
            col.min + 1,
            col.max + 1
        ));
        for (name, value) in &col.attrs {
            content.push_str(&format!(" {name}=\"{value}\""));
        }
        content.push_str("/>");
    }
    let new_section = if content.is_empty() {
        String::new()
    } else {
        format!("<cols>{content}</cols>")
    };
    if let Some(section) = section {
        let at = section.xml.as_ptr() as usize - xml.as_ptr() as usize;
        return Ok(format!(
            "{}{}{}",
            &xml[..at],
            new_section,
            &xml[at + section.xml.len()..]
        ));
    }
    if new_section.is_empty() {
        return Ok(xml.to_owned());
    }
    let data = root_children
        .iter()
        .find(|element| element.name == "sheetData")
        .ok_or("Worksheet has no sheetData element.")?;
    let at = data.xml.as_ptr() as usize - xml.as_ptr() as usize;
    Ok(format!("{}{}{}", &xml[..at], new_section, &xml[at..]))
}

pub(super) fn apply_outline_properties(
    xml: &str,
    op: &serde_json::Value,
) -> Result<String, String> {
    let below = op
        .get("summaryBelow")
        .and_then(serde_json::Value::as_bool)
        .ok_or("Invalid outline summaryBelow.")?;
    let right = op
        .get("summaryRight")
        .and_then(serde_json::Value::as_bool)
        .ok_or("Invalid outline summaryRight.")?;
    let root = super::style_entries::children(xml)
        .into_iter()
        .next()
        .ok_or("Worksheet has no root element.")?;
    let sections = super::style_entries::children(root.inner);
    let properties = sections.iter().find(|e| e.name == "sheetPr");
    let existing = properties.and_then(|p| {
        super::style_entries::children(p.inner)
            .into_iter()
            .find(|e| e.name == "outlinePr")
    });
    let mut element = existing
        .map(|e| e.xml.to_owned())
        .unwrap_or_else(|| "<outlinePr/>".into());
    element = super::sheets::set_attribute(&element, "summaryBelow", None);
    element = super::sheets::set_attribute(&element, "summaryRight", None);
    if !below {
        element = append_attribute(&element, "summaryBelow", "0");
    }
    if !right {
        element = append_attribute(&element, "summaryRight", "0");
    }
    let opening_end = element.find('>').ok_or("Malformed outline properties.")?;
    let attrs = element["<outlinePr".len()..opening_end]
        .trim_end_matches('/')
        .trim();
    let element = if attrs.is_empty() {
        String::new()
    } else {
        format!("<outlinePr {attrs}/>")
    };
    let Some(properties) = properties else {
        if element.is_empty() {
            return Ok(xml.to_owned());
        }
        let start = root.inner.as_ptr() as usize - xml.as_ptr() as usize;
        return Ok(format!(
            "{}<sheetPr>{element}</sheetPr>{}",
            &xml[..start],
            &xml[start..]
        ));
    };
    let at = properties.xml.as_ptr() as usize - xml.as_ptr() as usize;
    let replacement = if let Some(existing) = existing {
        let local = existing.xml.as_ptr() as usize - properties.xml.as_ptr() as usize;
        format!(
            "{}{}{}",
            &properties.xml[..local],
            element,
            &properties.xml[local + existing.xml.len()..]
        )
    } else if element.is_empty() {
        properties.xml.to_owned()
    } else if properties.xml.ends_with("/>") {
        format!(
            "{}>{element}</sheetPr>",
            &properties.xml[..properties.xml.len() - 2]
        )
    } else if let Some(page) = super::style_entries::children(properties.inner)
        .into_iter()
        .find(|e| e.name == "pageSetUpPr")
    {
        let local = page.xml.as_ptr() as usize - properties.xml.as_ptr() as usize;
        format!(
            "{}{element}{}",
            &properties.xml[..local],
            &properties.xml[local..]
        )
    } else {
        properties
            .xml
            .replacen("</sheetPr>", &format!("{element}</sheetPr>"), 1)
    };
    let replacement = if replacement == "<sheetPr></sheetPr>" {
        ""
    } else {
        replacement.as_str()
    };
    Ok(format!(
        "{}{}{}",
        &xml[..at],
        replacement,
        &xml[at + properties.xml.len()..]
    ))
}

pub(super) fn sync_outline_levels(xml: &str) -> Result<String, String> {
    let mut elements = Vec::new();
    super::sheets::descendants(xml, &mut elements);
    let mut max_row = 0u64;
    let mut max_col = 0u64;
    for element in &elements {
        let level = super::styles::attr(element.xml, "outlineLevel")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        if element.name == "row" {
            max_row = max_row.max(level);
        }
        if element.name == "col" {
            max_col = max_col.max(level);
        }
    }
    if let Some(format) = elements.iter().find(|e| e.name == "sheetFormatPr") {
        let mut replacement = super::sheets::set_attribute(format.xml, "outlineLevelRow", None);
        replacement = super::sheets::set_attribute(&replacement, "outlineLevelCol", None);
        if max_row > 0 {
            replacement = append_attribute(&replacement, "outlineLevelRow", &max_row.to_string());
        }
        if max_col > 0 {
            replacement = append_attribute(&replacement, "outlineLevelCol", &max_col.to_string());
        }
        let at = format.xml.as_ptr() as usize - xml.as_ptr() as usize;
        return Ok(format!(
            "{}{}{}",
            &xml[..at],
            replacement,
            &xml[at + format.xml.len()..]
        ));
    }
    if max_row == 0 && max_col == 0 {
        return Ok(xml.to_owned());
    }
    let Some(anchor) = elements
        .iter()
        .find(|e| e.name == "cols" || e.name == "sheetData")
    else {
        return Ok(xml.to_owned());
    };
    let mut format = "<sheetFormatPr defaultRowHeight=\"15\"/>".to_owned();
    if max_row > 0 {
        format = append_attribute(&format, "outlineLevelRow", &max_row.to_string());
    }
    if max_col > 0 {
        format = append_attribute(&format, "outlineLevelCol", &max_col.to_string());
    }
    let at = anchor.xml.as_ptr() as usize - xml.as_ptr() as usize;
    Ok(format!("{}{format}{}", &xml[..at], &xml[at..]))
}

pub(super) fn apply_merge(xml: &str, op: &serde_json::Value) -> Result<String, String> {
    let range = op.get("range").ok_or("Merge operation has no range.")?;
    let from = super::cells::address(
        super::cells::coordinate(range, "startRow")? + 1,
        super::cells::coordinate(range, "startColumn")?,
    );
    let to = super::cells::address(
        super::cells::coordinate(range, "endRow")? + 1,
        super::cells::coordinate(range, "endColumn")?,
    );
    let reference = format!("{from}:{to}");
    let kind = op
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or("Merge operation has no kind.")?;
    let root = super::style_entries::children(xml)
        .into_iter()
        .next()
        .ok_or("Worksheet has no sheetData element.")?;
    let children = super::style_entries::children(root.inner);
    let section = children.iter().find(|child| child.name == "mergeCells");
    if kind == "unmerge-cells" {
        let Some(section) = section else {
            return Ok(xml.to_owned());
        };
        let mut result_inner = String::new();
        let mut cursor = 0;
        let mut removed = false;
        let mut count = 0;
        for merge in super::style_entries::children(section.inner) {
            let at = merge.xml.as_ptr() as usize - section.inner.as_ptr() as usize;
            result_inner.push_str(&section.inner[cursor..at]);
            if !removed
                && merge.name == "mergeCell"
                && super::styles::attr(merge.xml, "ref") == Some(reference.as_str())
            {
                removed = true;
            } else {
                if merge.name == "mergeCell" {
                    count += 1;
                }
                result_inner.push_str(merge.xml);
            }
            cursor = at + merge.xml.len();
        }
        result_inner.push_str(&section.inner[cursor..]);
        let at = section.xml.as_ptr() as usize - xml.as_ptr() as usize;
        if count == 0 {
            return Ok(format!("{}{}", &xml[..at], &xml[at + section.xml.len()..]));
        }
        let open_end = section
            .xml
            .find('>')
            .ok_or("Malformed mergeCells element.")?
            + 1;
        let mut open = section.xml[..open_end].to_owned();
        if let Some(old) = super::styles::attr(&open, "count") {
            open = open.replacen(
                &format!("count=\"{old}\""),
                &format!("count=\"{count}\""),
                1,
            );
        }
        return Ok(format!(
            "{}{}{}{}</mergeCells>{}",
            &xml[..at],
            open,
            result_inner,
            "",
            &xml[at + section.xml.len()..]
        ));
    }
    if kind != "merge-cells" {
        return Err("Invalid merge operation.".into());
    }
    let new_merge = format!("<mergeCell ref=\"{reference}\"/>");
    if let Some(section) = section.filter(|section| !section.xml.ends_with("/>")) {
        let count = super::style_entries::children(section.inner)
            .iter()
            .filter(|e| e.name == "mergeCell")
            .count()
            + 1;
        let at = section.xml.as_ptr() as usize - xml.as_ptr() as usize;
        let open_end = section
            .xml
            .find('>')
            .ok_or("Malformed mergeCells element.")?
            + 1;
        let mut open = section.xml[..open_end].to_owned();
        if let Some(old) = super::styles::attr(&open, "count") {
            open = open.replacen(
                &format!("count=\"{old}\""),
                &format!("count=\"{count}\""),
                1,
            );
        }
        return Ok(format!(
            "{}{}{}{new_merge}</mergeCells>{}",
            &xml[..at],
            open,
            section.inner,
            &xml[at + section.xml.len()..]
        ));
    }
    let anchor = children
        .iter()
        .find(|child| child.name == "autoFilter")
        .or_else(|| children.iter().find(|child| child.name == "sheetData"))
        .ok_or("Worksheet has no sheetData element.")?;
    let at = anchor.xml.as_ptr() as usize - xml.as_ptr() as usize + anchor.xml.len();
    Ok(format!(
        "{}<mergeCells count=\"1\">{new_merge}</mergeCells>{}",
        &xml[..at],
        &xml[at..]
    ))
}

fn shift_vml_values(values: &mut [f64], axis: Axis, shift: Shift) -> Result<(), String> {
    let first = if axis == Axis::Row { 2 } else { 0 };
    let second = first + 4;
    let number = |n: i64| {
        i32::try_from(n)
            .map(f64::from)
            .map_err(|_| "Structural coordinate is too large.".to_owned())
    };
    match shift {
        Shift::Swap {
            first: a,
            second: b,
        } => {
            let (start, end) = (values[first], values[second]);
            let (a0, a1, b0, b1) = (
                number(a.start)?,
                number(a.end)?,
                number(b.start)?,
                number(b.end)?,
            );
            if end < a0 || start > b1 || (start <= a0 && end >= b1) {
                return Ok(());
            }
            if start >= a0 && end <= a1 {
                values[first] += b1 - b0 + 1.0;
                values[second] += b1 - b0 + 1.0;
            } else if start >= b0 && end <= b1 {
                values[first] -= a1 - a0 + 1.0;
                values[second] -= a1 - a0 + 1.0;
            } else {
                return Err(
                    "A range partially overlaps the moved rows — the move cannot be saved.".into(),
                );
            }
        }
        Shift::Insert { boundary, count } => {
            let (boundary, count) = (number(boundary)?, number(count)?);
            for at in [first, second] {
                if values[at] >= boundary {
                    values[at] += count;
                }
            }
        }
        Shift::Delete { deleted } => {
            let start = number(deleted.start)?;
            let end = number(deleted.end)?;
            for at in [first, second] {
                if values[at] > end {
                    values[at] -= end - start + 1.0;
                } else if values[at] >= start {
                    values[at] = start;
                    values[at + 1] = 0.0;
                }
            }
        }
    }
    Ok(())
}
pub(super) fn shift_vml_object_anchors(
    xml: &str,
    ops: &[serde_json::Value],
) -> Result<String, String> {
    if !xml.contains("ObjectType=\"Pict\"") {
        return Ok(xml.to_owned());
    }
    let mut nodes = Vec::new();
    super::sheets::descendants(xml, &mut nodes);
    let mut changes = Vec::new();
    for shape in nodes.into_iter().filter(|e| e.name == "v:shape") {
        let mut children = Vec::new();
        super::sheets::descendants(shape.inner, &mut children);
        if !children.iter().any(|e| {
            e.name == "x:ClientData" && super::styles::attr(e.xml, "ObjectType") == Some("Pict")
        }) {
            continue;
        }
        let Some(anchor) = children.iter().find(|e| e.name == "x:Anchor") else {
            continue;
        };
        let mut values = anchor
            .inner
            .split(',')
            .map(jscompat::to_number)
            .collect::<Vec<_>>();
        if values.len() != 8 || values.iter().any(|n| !n.is_finite()) {
            continue;
        }
        for op in ops {
            if let Some((axis, shift)) = parse_shift(op)? {
                shift_vml_values(&mut values, axis, shift)?;
            }
        }
        let replacement = values
            .into_iter()
            .map(jscompat::number_to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let start = anchor.inner.as_ptr() as usize - xml.as_ptr() as usize;
        changes.push((start, start + anchor.inner.len(), replacement));
    }
    let mut result = xml.to_owned();
    for (start, end, value) in changes.into_iter().rev() {
        result.replace_range(start..end, &value);
    }
    Ok(result)
}

pub(super) struct PendingTableSync {
    sheet_name: String,
    part_path: String,
    insertions: Vec<super::tables::TableColumnInsertion>,
}

fn structural_sheet(
    xml: &str,
    sheet: &str,
    ops: &[serde_json::Value],
    mut stylesheet: Option<&mut super::styles::StylesheetEditor>,
) -> Result<String, String> {
    if ops.is_empty() {
        return Ok(xml.to_owned());
    }
    let mut xml = super::cells::infer_addresses(&super::normalize::canonical_xml_spelling(xml)?)?;
    let mut outline = false;
    for op in ops {
        let kind = op
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or("Structural operation has no kind.")?;
        match kind {
            "set-row-size" | "set-rows-hidden" | "set-rows-outline" => {
                outline |= kind == "set-rows-outline";
                xml = apply_row_attributes(&xml, op)?;
            }
            "set-col-size" | "set-cols-hidden" | "set-cols-outline" | "set-col-style" => {
                outline |= kind == "set-cols-outline";
                xml = apply_col_attributes(&xml, op, stylesheet.as_deref_mut())?;
            }
            "set-outline-pr" => xml = apply_outline_properties(&xml, op)?,
            "merge-cells" | "unmerge-cells" => xml = apply_merge(&xml, op)?,
            "insert-rows" | "remove-rows" | "move-rows" | "insert-cols" | "remove-cols" => {
                let (axis, shift) = parse_shift(op)?.ok_or("Invalid structural shift.")?;
                xml = materialize_broken_shared_formulas(&xml, sheet, shift, axis, true)?;
                xml = shift_sheet_positions(&xml, shift, axis)?;
                xml = shift_sheet_formulas(&xml, sheet, shift, axis)?;
                xml = shift_ranged_features(&xml, shift, axis)?;
                if axis == Axis::Column {
                    xml = shift_column_definitions(&xml, shift)?;
                }
            }
            _ => return Err(format!("Invalid structural operation: {kind}.")),
        }
    }
    if outline {
        xml = sync_outline_levels(&xml)?;
    }
    shift_ole_anchors(&xml, ops)
}

fn shifting(ops: &[serde_json::Value]) -> bool {
    ops.iter()
        .any(|op| op.get("index").is_some() || op.get("range").is_some())
}
fn part_rels_path(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, name)) => format!("{dir}/_rels/{name}.rels"),
        None => format!("_rels/{path}.rels"),
    }
}
fn pivot_reads_sheet(xml: &str, name: &str) -> bool {
    let mut nodes = Vec::new();
    super::sheets::descendants(xml, &mut nodes);
    nodes
        .into_iter()
        .filter(|node| node.name == "worksheetSource")
        .filter_map(|node| super::styles::attr(node.xml, "sheet"))
        .any(|value| super::cells::decode_xml(value).to_lowercase() == name.to_lowercase())
}

fn shift_anchored_parts(
    pkg: &mut super::package::PackageEditor<'_>,
    sheet_path: &str,
    sheet_xml: &str,
    sheet_name: &str,
    ops: &[serde_json::Value],
    touched: &mut std::collections::HashSet<String>,
    pending: &mut Vec<PendingTableSync>,
) -> Result<(), String> {
    if !shifting(ops) {
        return Ok(());
    }
    let mut nodes = Vec::new();
    super::sheets::descendants(sheet_xml, &mut nodes);
    let mut parts = Vec::new();
    if let Some(id) = nodes
        .iter()
        .find(|e| e.name == "drawing")
        .and_then(|e| super::styles::attr(e.xml, "r:id"))
    {
        parts.push((id, "drawing"));
    }
    for node in nodes.iter().filter(|e| e.name == "tablePart") {
        if let Some(id) = super::styles::attr(node.xml, "r:id") {
            parts.push((id, "table"));
        }
    }
    if sheet_xml.contains("<oleObjects")
        && let Some(id) = nodes
            .iter()
            .find(|e| e.name == "legacyDrawing")
            .and_then(|e| super::styles::attr(e.xml, "r:id"))
        {
            parts.push((id, "vml"));
        }
    if parts.is_empty() {
        return Ok(());
    }
    let rels_path = part_rels_path(sheet_path);
    if !pkg.has(&rels_path)? {
        return Err(format!(
            "{sheet_path} has anchored parts but {rels_path} is missing — rows/columns cannot shift here."
        ));
    }
    let rels = pkg.read_text(&rels_path)?;
    let mut relationships = Vec::new();
    super::sheets::descendants(&rels, &mut relationships);
    for (id, kind) in parts {
        let target = relationships.iter().find(|rel| rel.name == "Relationship" && super::styles::attr(rel.xml, "Id") == Some(id))
            .and_then(|rel| super::styles::attr(rel.xml, "Target"))
            .ok_or_else(|| format!("{sheet_path} references {kind} {id} but its relationship is missing — rows/columns cannot shift here."))?;
        let target = super::cells::decode_xml(target);
        let path = super::state::resolve_rel_target(sheet_path, &target)?;
        if !pkg.can_patch(&path)? {
            return Err(format!(
                "{path} is too large to rewrite — rows/columns cannot shift here."
            ));
        }
        let xml = pkg.read_text(&path)?;
        let shifted = match kind {
            "drawing" => shift_drawing_anchors(&xml, ops)?,
            "vml" => shift_vml_object_anchors(&xml, ops)?,
            "table" => {
                let (shifted, insertions) = super::tables::shift_table_part(&xml, ops)?;
                if !insertions.is_empty() {
                    pending.push(PendingTableSync {
                        sheet_name: sheet_name.into(),
                        part_path: path.clone(),
                        insertions,
                    });
                }
                shifted
            }
            _ => unreachable!(),
        };
        if shifted != *xml {
            pkg.write(&path, shifted)?;
            touched.insert(path);
        }
    }
    Ok(())
}

pub(super) fn apply(
    pkg: &mut super::package::PackageEditor<'_>,
    state: &mut super::state::PlannerState,
    resolved: &serde_json::Value,
    mut stylesheet: Option<&mut super::styles::StylesheetEditor>,
    touched: &mut std::collections::HashSet<String>,
) -> Result<Vec<PendingTableSync>, String> {
    let mut pending = Vec::new();
    let Some(groups) = resolved
        .get("structuralOps")
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(pending);
    };
    let pivot_paths = if groups.iter().any(|group| {
        group
            .get("ops")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|ops| shifting(ops))
    }) {
        pkg.paths()?
            .into_iter()
            .filter(|path| {
                path.starts_with("xl/pivotCache/pivotCacheDefinition")
                    && path.ends_with(".xml")
                    && !path["xl/pivotCache/".len()..].contains('/')
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    for group in groups {
        let name = group
            .get("sheetName")
            .and_then(serde_json::Value::as_str)
            .ok_or("Structural operation has no sheetName.")?;
        let ops = group
            .get("ops")
            .and_then(serde_json::Value::as_array)
            .ok_or("Structural operation has no ops.")?;
        if ops.is_empty() {
            continue;
        }
        if shifting(ops) {
            for path in &pivot_paths {
                if pivot_reads_sheet(&pkg.read_text(path)?, name) {
                    return Err(format!(
                        "A pivot table reads its source data from \"{name}\" — row/column changes there cannot be saved."
                    ));
                }
            }
        }
        let sheet = state
            .worksheet_mut(name)
            .ok_or_else(|| format!("Sheet \"{name}\" was not loaded."))?;
        let path = sheet.path.clone();
        sheet.xml = std::sync::Arc::new(structural_sheet(
            &sheet.xml,
            name,
            ops,
            stylesheet.as_deref_mut(),
        )?);
        shift_anchored_parts(pkg, &path, &sheet.xml, name, ops, touched, &mut pending)?;
        for other_path in pkg.paths()? {
            let chart = other_path.starts_with("xl/charts/") && other_path.ends_with(".xml");
            let other_sheet = other_path.starts_with("xl/worksheets/")
                && other_path.ends_with(".xml")
                && other_path != path;
            if !chart && !other_sheet {
                continue;
            }
            let index = state
                .worksheets
                .iter()
                .position(|entry| entry.path == other_path);
            if index.is_none() && !pkg.can_patch(&other_path)? {
                if pkg.contains_text(&other_path, name)? {
                    return Err(format!(
                        "{other_path} references \"{name}\" but is too large to rewrite — this structural change cannot be saved."
                    ));
                }
                continue;
            }
            let original = if let Some(index) = index {
                state.worksheets[index].xml.clone()
            } else {
                pkg.read_text(&other_path)?
            };
            let mut shifted = original.to_string();
            for op in ops {
                if let Some((axis, shift)) = parse_shift(op)? {
                    if !chart {
                        shifted =
                            materialize_broken_shared_formulas(&shifted, name, shift, axis, false)?;
                    }
                    shifted = shift_qualified_formulas(
                        &shifted,
                        name,
                        shift,
                        axis,
                        if chart {
                            FormulaPart::Chart
                        } else {
                            FormulaPart::Worksheet
                        },
                    )?;
                }
            }
            if shifted != *original {
                if let Some(index) = index {
                    state.worksheets[index].xml = std::sync::Arc::new(shifted);
                } else {
                    pkg.write(&other_path, shifted)?;
                    touched.insert(other_path);
                }
            }
        }
        let mut workbook = state.workbook_xml.to_string();
        for op in ops {
            if let Some((axis, shift)) = parse_shift(op)? {
                workbook =
                    shift_qualified_formulas(&workbook, name, shift, axis, FormulaPart::Workbook)?;
            }
        }
        if workbook != *state.workbook_xml {
            state.workbook_xml = std::sync::Arc::new(workbook);
            pkg.write_shared("xl/workbook.xml", state.workbook_xml.clone())?;
            touched.insert("xl/workbook.xml".into());
        }
    }
    Ok(pending)
}

fn decode_cell_text(raw: &str) -> String {
    let xml = super::cells::decode_xml(raw)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let bytes = xml.as_bytes();
    let mut out = String::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'_'
            && bytes.get(cursor + 1) == Some(&b'x')
            && bytes.get(cursor + 6) == Some(&b'_')
            && let Some(hex) = xml.get(cursor + 2..cursor + 6)
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
            && let Ok(code) = u32::from_str_radix(hex, 16)
            && let Some(ch) = char::from_u32(code)
        {
                            out.push(ch);
                            cursor += 7;
                            continue;
                        }
        let Some(ch) = xml[cursor..].chars().next() else {
            break;
        };
        out.push(ch);
        cursor += ch.len_utf8();
    }
    out.replace("\r\n", "\n").replace('\r', "\n")
}

fn shared_strings(xml: &str) -> Vec<String> {
    let mut nodes = Vec::new();
    super::sheets::descendants(xml, &mut nodes);
    nodes
        .into_iter()
        .filter(|node| node.name == "si")
        .map(|item| {
            let mut children = Vec::new();
            super::sheets::descendants(item.inner, &mut children);
            children
                .into_iter()
                .filter(|node| node.name == "t" && !node.xml.ends_with("/>"))
                .map(|node| decode_cell_text(node.inner))
                .collect()
        })
        .collect()
}
enum Header {
    Blank,
    Opaque,
    Text { text: String, plain: bool },
}
fn header_value(xml: &str, address: &str, strings: &[String]) -> Header {
    let mut nodes = Vec::new();
    super::sheets::descendants(xml, &mut nodes);
    let Some(cell) = nodes
        .into_iter()
        .find(|node| node.name == "c" && super::styles::attr(node.xml, "r") == Some(address))
    else {
        return Header::Blank;
    };
    let mut content = Vec::new();
    super::sheets::descendants(cell.inner, &mut content);
    if content.iter().any(|node| node.name == "f") {
        return Header::Opaque;
    }
    let kind = super::styles::attr(cell.xml, "t");
    if kind == Some("e") {
        return Header::Opaque;
    }
    if kind == Some("inlineStr") {
        let text: String = content
            .iter()
            .filter(|node| node.name == "t" && !node.xml.ends_with("/>"))
            .map(|node| decode_cell_text(node.inner))
            .collect();
        return if text.is_empty() {
            Header::Blank
        } else {
            Header::Text { text, plain: true }
        };
    }
    let Some(raw) = content
        .iter()
        .find(|node| node.name == "v" && !node.xml.ends_with("/>"))
        .map(|node| node.inner)
    else {
        return Header::Blank;
    };
    if kind == Some("s") {
        let text = raw
            .parse::<usize>()
            .ok()
            .and_then(|index| strings.get(index))
            .cloned()
            .unwrap_or_default();
        return if text.is_empty() {
            Header::Blank
        } else {
            Header::Text { text, plain: true }
        };
    }
    if kind == Some("b") {
        return Header::Text {
            text: if raw == "1" {
                "TRUE".into()
            } else {
                "FALSE".into()
            },
            plain: false,
        };
    }
    let text = if kind == Some("str") {
        decode_cell_text(raw)
    } else {
        super::cells::decode_xml(raw)
    };
    if text.is_empty() {
        Header::Blank
    } else {
        Header::Text {
            text,
            plain: kind == Some("str"),
        }
    }
}
fn write_header(xml: &str, row: usize, column: usize, text: &str) -> Result<String, String> {
    let address = super::cells::address(row + 1, column);
    let mut nodes = Vec::new();
    super::sheets::descendants(xml, &mut nodes);
    let existing = nodes.iter().find(|node| {
        node.name == "c" && super::styles::attr(node.xml, "r") == Some(address.as_str())
    });
    let style = existing.and_then(|cell| super::styles::attr(cell.xml, "s"));
    let value = serde_json::json!({"value":text});
    let replacement = super::cells::serialize_styled_cell(&address, &value, style, None, None)?;
    if let Some(cell) = existing {
        let at = cell.xml.as_ptr() as usize - xml.as_ptr() as usize;
        return Ok(format!(
            "{}{}{}",
            &xml[..at],
            replacement,
            &xml[at + cell.xml.len()..]
        ));
    }
    if let Some(target_row) = nodes.iter().find(|node| {
        node.name == "row"
            && super::styles::attr(node.xml, "r").and_then(|v| v.parse::<usize>().ok())
                == Some(row + 1)
    }) {
        let at = target_row.xml.as_ptr() as usize - xml.as_ptr() as usize;
        if target_row.xml.ends_with("/>") {
            let opened = &target_row.xml[..target_row.xml.len() - 2];
            return Ok(format!(
                "{}{}>{replacement}</row>{}",
                &xml[..at],
                opened,
                &xml[at + target_row.xml.len()..]
            ));
        }
        let siblings = super::style_entries::children(target_row.inner);
        let column_number = i64::try_from(column).map_err(|_| "Invalid table header column.")?;
        let insert = siblings
            .iter()
            .find(|cell| {
                cell.name == "c"
                    && super::styles::attr(cell.xml, "r")
                        .and_then(parse_cell)
                        .is_some_and(|coord| coord.column > column_number)
            })
            .map(|cell| cell.xml.as_ptr() as usize - xml.as_ptr() as usize)
            .unwrap_or(
                target_row.inner.as_ptr() as usize - xml.as_ptr() as usize + target_row.inner.len(),
            );
        return Ok(format!("{}{replacement}{}", &xml[..insert], &xml[insert..]));
    }
    let data = nodes
        .iter()
        .find(|node| node.name == "sheetData")
        .ok_or("Worksheet has no sheetData element.")?;
    let at = data.xml.as_ptr() as usize - xml.as_ptr() as usize;
    let new_row = format!("<row r=\"{}\">{replacement}</row>", row + 1);
    if data.xml.ends_with("/>") {
        let opened = &data.xml[..data.xml.len() - 2];
        Ok(format!(
            "{}{}>{new_row}</sheetData>{}",
            &xml[..at],
            opened,
            &xml[at + data.xml.len()..]
        ))
    } else {
        let insert = data.inner.as_ptr() as usize - xml.as_ptr() as usize + data.inner.len();
        Ok(format!("{}{new_row}{}", &xml[..insert], &xml[insert..]))
    }
}
fn rename_table_column(xml: &str, id: u64, name: &str) -> String {
    let mut nodes = Vec::new();
    super::sheets::descendants(xml, &mut nodes);
    if let Some(column) = nodes.into_iter().find(|node| {
        node.name == "tableColumn"
            && super::styles::attr(node.xml, "id").and_then(|s| s.parse::<u64>().ok()) == Some(id)
    }) && super::styles::attr(column.xml, "name").is_some()
    {
            let start = column.xml.as_ptr() as usize - xml.as_ptr() as usize;
            let replacement = super::sheets::set_attribute(column.xml, "name", Some(name));
            return format!(
                "{}{}{}",
                &xml[..start],
                replacement,
                &xml[start + column.xml.len()..]
            );
        }
    xml.to_owned()
}

pub(super) fn sync_tables(
    pkg: &mut super::package::PackageEditor<'_>,
    state: &mut super::state::PlannerState,
    pending: Vec<PendingTableSync>,
    touched: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    if pending.is_empty() {
        return Ok(());
    }
    let strings = if pkg.has("xl/sharedStrings.xml")? {
        shared_strings(&pkg.read_text("xl/sharedStrings.xml")?)
    } else {
        Vec::new()
    };
    for sync in pending {
        let Some(sheet) = state.worksheet_mut(&sync.sheet_name) else {
            continue;
        };
        let mut worksheet = sheet.xml.to_string();
        let mut table = pkg.read_text(&sync.part_path)?.to_string();
        let mut nodes = Vec::new();
        super::sheets::descendants(&table, &mut nodes);
        let mut names: std::collections::HashSet<String> = nodes
            .into_iter()
            .filter(|node| node.name == "tableColumn")
            .filter_map(|node| super::styles::attr(node.xml, "name"))
            .map(|name| super::cells::decode_xml(name).to_lowercase())
            .collect();
        for insertion in sync.insertions {
            let Some(row) = insertion.header_row else {
                continue;
            };
            let row = usize::try_from(row).map_err(|_| "Invalid table header row.")?;
            for column in insertion.columns {
                let index =
                    usize::try_from(column.column).map_err(|_| "Invalid table header column.")?;
                let address = super::cells::address(row + 1, index);
                match header_value(&worksheet, &address, &strings) {
                    Header::Blank => {
                        worksheet = write_header(&worksheet, row, index, &column.name)?
                    }
                    Header::Opaque => {}
                    Header::Text { text, plain } => {
                        let available = |candidate: &str| {
                            candidate.to_lowercase() == column.name.to_lowercase()
                                || !names.contains(&candidate.to_lowercase())
                        };
                        let mut unique = text.clone();
                        if !available(&unique) {
                            let mut suffix = 2usize;
                            loop {
                                unique = format!("{text}{suffix}");
                                if available(&unique) {
                                    break;
                                }
                                suffix += 1;
                            }
                        }
                        if !plain && unique != text {
                            continue;
                        }
                        if unique.to_lowercase() != column.name.to_lowercase() {
                            names.remove(&column.name.to_lowercase());
                            names.insert(unique.to_lowercase());
                            table = rename_table_column(&table, column.id, &unique);
                        }
                        if plain && unique != text {
                            worksheet = write_header(&worksheet, row, index, &unique)?;
                        }
                    }
                }
            }
        }
        sheet.xml = std::sync::Arc::new(worksheet);
        pkg.write(&sync.part_path, table)?;
        touched.insert(sync.part_path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_shrinks_or_removes_ranges() {
        let shift = Shift::Delete {
            deleted: Span { start: 2, end: 4 },
        };
        assert_eq!(
            move_range(Span { start: 1, end: 7 }, shift).unwrap(),
            Some(Span { start: 1, end: 4 })
        );
        assert_eq!(move_range(Span { start: 2, end: 4 }, shift).unwrap(), None);
        assert_eq!(move_position(6, shift), Some(3));
    }

    #[test]
    fn moved_blocks_reject_torn_ranges() {
        let shift = Shift::Swap {
            first: Span { start: 1, end: 2 },
            second: Span { start: 3, end: 5 },
        };
        assert_eq!(
            move_range(Span { start: 1, end: 2 }, shift).unwrap(),
            Some(Span { start: 4, end: 5 })
        );
        assert!(move_range(Span { start: 2, end: 3 }, shift).is_err());
    }
    #[test]
    fn shifts_formula_ranges_but_not_other_sheets_or_literals() {
        let shift = Shift::Insert {
            boundary: 1,
            count: 2,
        };
        let result = shift_formula_text(
            "SUM(Data!$A$1:A3)+Other!A2+\"A2\"",
            "Data",
            shift,
            Axis::Row,
            true,
        )
        .unwrap();
        assert_eq!(result, "SUM(Data!$A$1:A5)+Other!A2+\"A2\"");
        assert!(
            shift_formula_text(
                "A3",
                "Data",
                Shift::Delete {
                    deleted: Span { start: 2, end: 2 }
                },
                Axis::Row,
                false
            )
            .is_err()
        );
    }
    #[test]
    fn insertion_and_move_keep_cells_with_their_rows() {
        let xml = "<worksheet><sheetData><row r=\"1\"><c r=\"A1\"><v>1</v></c></row><row r=\"2\"><c r=\"B2\"><v>2</v></c></row></sheetData></worksheet>";
        let inserted = shift_sheet_positions(
            xml,
            Shift::Insert {
                boundary: 1,
                count: 2,
            },
            Axis::Row,
        )
        .unwrap();
        assert!(inserted.contains("<row r=\"4\"><c r=\"B4\"><v>2</v></c></row>"));
        let moved = shift_sheet_positions(
            xml,
            Shift::Swap {
                first: Span { start: 0, end: 0 },
                second: Span { start: 1, end: 1 },
            },
            Axis::Row,
        )
        .unwrap();
        assert!(moved.contains(
            "<row r=\"1\"><c r=\"B1\"><v>2</v></c></row><row r=\"2\"><c r=\"A2\"><v>1</v></c></row>"
        ));
    }
    #[test]
    fn structural_replay_moves_cells_and_formula_ranges_together() {
        let xml = "<worksheet><dimension ref=\"A1:B2\"/><sheetData><row r=\"1\"><c r=\"A1\"><f>SUM(B1:B2)</f><v>3</v></c></row><row r=\"2\"><c r=\"B2\"><v>2</v></c></row></sheetData><mergeCells count=\"1\"><mergeCell ref=\"A1:A2\"/></mergeCells></worksheet>";
        let ops = [serde_json::json!({"kind":"insert-rows","index":1,"count":2})];
        let result = structural_sheet(xml, "Data", &ops, None).unwrap();
        assert!(result.contains("<f>SUM(B1:B4)</f>"));
        assert!(result.contains("<c r=\"B4\"><v>2</v></c>"));
        assert!(result.contains("<mergeCell ref=\"A1:A4\"/>"));
    }
    #[test]
    fn vml_clamps_deleted_ole_anchor_without_moving_note_shape() {
        let xml = "<xml><v:shape><x:ClientData ObjectType=\"Pict\"><x:Anchor>0, 3, 2, 8, 4, 7, 5, 9</x:Anchor></x:ClientData></v:shape><v:shape><x:ClientData ObjectType=\"Note\"><x:Anchor>0, 3, 2, 8, 4, 7, 5, 9</x:Anchor></x:ClientData></v:shape></xml>";
        let ops = [serde_json::json!({"kind":"remove-rows","index":2,"count":2})];
        let result = shift_vml_object_anchors(xml, &ops).unwrap();
        assert!(result.contains("<x:Anchor>0, 3, 2, 0, 4, 7, 3, 9</x:Anchor>"));
        assert!(result.contains("<x:Anchor>0, 3, 2, 8, 4, 7, 5, 9</x:Anchor>"));
    }
}
