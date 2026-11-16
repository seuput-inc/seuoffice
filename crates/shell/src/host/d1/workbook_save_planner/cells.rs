//! Serialization of worksheet cell values and inline rich text.
use super::formulas::{spills_dynamic_array, with_future_function_markers};
use serde_json::Value;

pub(super) fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_attribute(value: &str) -> String {
    xml_text(value)
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn cell_text(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for (i, c) in value.char_indices() {
        let remaining = &value[i..];
        let escaped_literal = c == '_'
            && remaining.as_bytes().get(1) == Some(&b'x')
            && remaining
                .as_bytes()
                .get(2..6)
                .is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit))
            && remaining.as_bytes().get(6) == Some(&b'_');
        if escaped_literal {
            encoded.push_str("_x005F_");
        } else if matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\r' | '\u{fffe}' | '\u{ffff}')
        {
            encoded.push_str(&format!("_x{:04X}_", c as u32));
        } else {
            encoded.push(c);
        }
    }
    xml_text(&encoded)
}

fn run_properties(run: &Value) -> String {
    let mut parts = String::new();
    for (name, tag) in [
        ("bold", "b"),
        ("italic", "i"),
        ("strikethrough", "strike"),
        ("underline", "u"),
    ] {
        if run.get(name).and_then(Value::as_bool) == Some(true) {
            parts.push_str(&format!("<{tag}/>"));
        }
    }
    if let Some(size) = run.get("size").filter(|v| v.is_number()) {
        parts.push_str(&format!("<sz val=\"{size}\"/>"));
    }
    if let Some(color) = run.get("color").and_then(Value::as_str) {
        let hex = color
            .strip_prefix('#')
            .unwrap_or(color)
            .to_ascii_uppercase();
        if hex.len() == 6 && hex.bytes().all(|c| c.is_ascii_hexdigit()) {
            parts.push_str(&format!("<color rgb=\"FF{hex}\"/>"));
        }
    }
    if let Some(family) = run.get("family").and_then(Value::as_str) {
        parts.push_str(&format!("<rFont val=\"{}\"/>", xml_attribute(family)));
    }
    if let Some(align) = run.get("vertAlign").and_then(Value::as_str) {
        parts.push_str(&format!("<vertAlign val=\"{}\"/>", xml_attribute(align)));
    }
    if parts.is_empty() {
        parts
    } else {
        format!("<rPr>{parts}</rPr>")
    }
}

pub(super) fn serialize_styled_cell(
    address: &str,
    cell: &Value,
    style: Option<&str>,
    rich: Option<&[Value]>,
    array_ref: Option<&str>,
) -> Result<String, String> {
    let style_attr = style
        .map(|s| format!(" s=\"{}\"", xml_attribute(s)))
        .unwrap_or_default();
    if let Some(formula) = cell
        .get("formula")
        .and_then(Value::as_str)
        .filter(|f| !f.is_empty())
    {
        let formula = formula.strip_prefix('=').unwrap_or(formula);
        let text = xml_text(&with_future_function_markers(formula));
        let extent = array_ref.or_else(|| spills_dynamic_array(formula).then_some(address));
        let formula_xml = match extent {
            Some(r) => format!("<f t=\"array\" ref=\"{}\">{text}</f>", xml_attribute(r)),
            None => format!("<f>{text}</f>"),
        };
        return Ok(format!("<c r=\"{address}\"{style_attr}>{formula_xml}</c>"));
    }
    let value = cell.get("value").unwrap_or(&Value::Null);
    match value {
        Value::Null => Ok(if style.is_some() {
            format!("<c r=\"{address}\"{style_attr}/>")
        } else {
            String::new()
        }),
        Value::String(text) => {
            let body = if let Some(runs) = rich.filter(|runs| !runs.is_empty()) {
                let mut body = String::new();
                for run in runs {
                    let text = run
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or("Rich text run has no text.")?;
                    body.push_str(&format!(
                        "<r>{}<t xml:space=\"preserve\">{}</t></r>",
                        run_properties(run),
                        cell_text(text)
                    ));
                }
                body
            } else {
                format!("<t xml:space=\"preserve\">{}</t>", cell_text(text))
            };
            Ok(format!(
                "<c r=\"{address}\"{style_attr} t=\"inlineStr\"><is>{body}</is></c>"
            ))
        }
        Value::Bool(v) => Ok(format!(
            "<c r=\"{address}\"{style_attr} t=\"b\"><v>{}</v></c>",
            if *v { 1 } else { 0 }
        )),
        Value::Number(v) => {
            let text = if v.is_f64() {
                jscompat::number_to_string(v.as_f64().ok_or("Invalid cell number.")?)
            } else {
                v.to_string()
            };
            Ok(format!("<c r=\"{address}\"{style_attr}><v>{text}</v></c>"))
        }
        _ => Err("Invalid cell value.".into()),
    }
}

fn is_grid_address(address: &str) -> bool {
    let letters = address.bytes().take_while(u8::is_ascii_uppercase).count();
    if !(1..=3).contains(&letters) || letters == address.len() {
        return false;
    }
    let mut column = 0usize;
    for b in address[..letters].bytes() {
        column = column * 26 + usize::from(b - b'A' + 1);
    }
    let digits = &address[letters..];
    !digits.starts_with('0')
        && digits.bytes().all(|b| b.is_ascii_digit())
        && digits
            .parse::<usize>()
            .is_ok_and(|row| (1..=1_048_576).contains(&row))
        && column <= 16_384
}

fn existing_array_ref(cell: &str) -> Option<&str> {
    let root = super::style_entries::children(cell).into_iter().next()?;
    let formula = super::style_entries::children(root.inner)
        .into_iter()
        .find(|e| e.name == "f" && super::styles::attr(e.xml, "t") == Some("array"))?;
    let reference = super::styles::attr(formula.xml, "ref")?;
    let mut parts = reference.split(':');
    let start = parts.next()?;
    if !is_grid_address(start) {
        return None;
    }
    if let Some(end) = parts.next() {
        if !is_grid_address(end) {
            return None;
        }
    }
    Some(reference)
}

pub(super) fn apply_edit_to_cell(
    cell_xml: &str,
    address: &str,
    edit: &Value,
    mut stylesheet: Option<&mut super::styles::StylesheetEditor>,
) -> Result<String, String> {
    let existing_style = super::styles::attr(cell_xml, "s");
    let style = edit.get("style").filter(|value| !value.is_null());
    let reset = edit.get("styleReset").and_then(Value::as_bool) == Some(true);
    let override_style = if reset {
        Some(
            if let (Some(delta), Some(editor)) = (style, stylesheet.as_deref_mut()) {
                editor.resolve_style(0, delta)?
            } else {
                0
            },
        )
    } else if let (Some(delta), Some(editor)) = (style, stylesheet.as_deref_mut()) {
        let base = existing_style
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        Some(editor.resolve_style(base, delta)?)
    } else {
        None
    };
    if edit.get("writeValue").and_then(Value::as_bool) == Some(true) {
        let override_text = override_style.map(|value| value.to_string());
        let rich = edit
            .get("rich")
            .and_then(Value::as_array)
            .map(Vec::as_slice);
        return serialize_styled_cell(
            address,
            edit.get("cell").ok_or("Cell edit has no cell.")?,
            override_text.as_deref().or(existing_style),
            rich,
            existing_array_ref(cell_xml),
        );
    }
    let Some(style) = override_style else {
        return Ok(cell_xml.to_owned());
    };
    if cell_xml.is_empty() {
        return Ok(format!("<c r=\"{address}\" s=\"{style}\"/>"));
    }
    if let Some(old) = existing_style {
        return Ok(cell_xml.replacen(&format!(" s=\"{old}\""), &format!(" s=\"{style}\""), 1));
    }
    Ok(cell_xml.replacen(
        &format!("r=\"{address}\""),
        &format!("r=\"{address}\" s=\"{style}\""),
        1,
    ))
}

#[derive(Default)]
struct CellMutation<'a> {
    fill: Option<&'a Value>,
    edits: Vec<&'a Value>,
}
type Mutations<'a> =
    std::collections::BTreeMap<usize, std::collections::BTreeMap<usize, CellMutation<'a>>>;

pub(super) fn coordinate(value: &Value, name: &str) -> Result<usize, String> {
    let number = value
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("Invalid cell {name}."))?;
    usize::try_from(number).map_err(|_| format!("Invalid cell {name}."))
}
pub(super) fn address(row: usize, column: usize) -> String {
    let mut n = column + 1;
    let mut letters = Vec::new();
    while n > 0 {
        letters.push((b'A' + ((n - 1) % 26) as u8) as char);
        n = (n - 1) / 26;
    }
    letters.reverse();
    format!("{}{row}", letters.into_iter().collect::<String>())
}
fn parse_address(value: &str) -> Option<(usize, usize)> {
    let n = value.bytes().take_while(u8::is_ascii_uppercase).count();
    if n == 0 || n > 3 || n == value.len() {
        return None;
    }
    let mut column = 0usize;
    for b in value[..n].bytes() {
        column = column
            .checked_mul(26)?
            .checked_add(usize::from(b - b'A' + 1))?;
    }
    let row = value[n..].parse::<usize>().ok().filter(|n| *n > 0)?;
    Some((row, column - 1))
}
fn add_attribute(xml: &str, key: &str, value: &str) -> String {
    let end = xml
        .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .unwrap_or(xml.len());
    format!("{} {key}=\"{value}\"{}", &xml[..end], &xml[end..])
}
fn apply_mutation(
    cell: &str,
    row: usize,
    column: usize,
    mutation: &CellMutation<'_>,
    stylesheet: &mut Option<&mut super::styles::StylesheetEditor>,
) -> Result<String, String> {
    let address = address(row, column);
    let mut result = cell.to_owned();
    if let Some(value) = mutation.fill {
        let edit = serde_json::json!({"writeValue":true,"cell":{"value":value}});
        result = apply_edit_to_cell(&result, &address, &edit, stylesheet.as_deref_mut())?;
    }
    for edit in &mutation.edits {
        result = apply_edit_to_cell(&result, &address, edit, stylesheet.as_deref_mut())?;
    }
    Ok(result)
}
fn build_row(
    row: usize,
    items: &std::collections::BTreeMap<usize, CellMutation<'_>>,
    stylesheet: &mut Option<&mut super::styles::StylesheetEditor>,
) -> Result<String, String> {
    let mut cells = String::new();
    for (column, mutation) in items {
        cells.push_str(&apply_mutation("", row, *column, mutation, stylesheet)?);
    }
    Ok(if cells.is_empty() {
        String::new()
    } else {
        format!("<row r=\"{row}\">{cells}</row>")
    })
}
fn edit_row(
    row_xml: &str,
    row: usize,
    items: &std::collections::BTreeMap<usize, CellMutation<'_>>,
    stylesheet: &mut Option<&mut super::styles::StylesheetEditor>,
) -> Result<String, String> {
    let root = super::style_entries::children(row_xml)
        .into_iter()
        .next()
        .ok_or("Malformed worksheet row.")?;
    let open_end = row_xml.find('>').ok_or("Malformed worksheet row.")? + 1;
    let self_closing = row_xml[..open_end].ends_with("/>");
    let mut open = if self_closing {
        format!("{}>", &row_xml[..open_end - 2])
    } else {
        row_xml[..open_end].to_owned()
    };
    if super::styles::attr(&open, "r").is_none() {
        open = add_attribute(&open, "r", &row.to_string());
    }
    let mut result = open;
    let mut pending = items.iter().peekable();
    let mut cursor = 0;
    let mut next_col = 0;
    for cell in super::style_entries::children(root.inner)
        .into_iter()
        .filter(|e| e.name == "c")
    {
        let start = cell.xml.as_ptr() as usize - root.inner.as_ptr() as usize;
        result.push_str(&root.inner[cursor..start]);
        let explicit = super::styles::attr(cell.xml, "r");
        let col = explicit
            .and_then(parse_address)
            .map(|(_, col)| col)
            .unwrap_or(next_col);
        next_col = col + 1;
        while pending.peek().is_some_and(|(key, _)| **key < col) {
            if let Some((key, mutation)) = pending.next() {
                result.push_str(&apply_mutation("", row, *key, mutation, stylesheet)?);
            }
        }
        let normalized;
        let cell_xml = if explicit.is_none() {
            normalized = add_attribute(cell.xml, "r", &address(row, col));
            normalized.as_str()
        } else {
            cell.xml
        };
        if pending.peek().is_some_and(|(key, _)| **key == col) {
            if let Some((_, mutation)) = pending.next() {
                result.push_str(&apply_mutation(cell_xml, row, col, mutation, stylesheet)?);
            }
        } else {
            result.push_str(cell_xml);
        }
        cursor = start + cell.xml.len();
    }
    result.push_str(&root.inner[cursor..]);
    for (col, mutation) in pending {
        result.push_str(&apply_mutation("", row, *col, mutation, stylesheet)?);
    }
    result.push_str("</row>");
    Ok(result)
}

pub(super) fn decode_xml(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find('&') {
        let start = cursor + relative;
        output.push_str(&text[cursor..start]);
        let Some(end) = text[start..].find(';').map(|n| start + n) else {
            output.push_str(&text[start..]);
            return output;
        };
        let entity = &text[start + 1..end];
        let character = match entity {
            "quot" => Some('"'),
            "apos" => Some('\''),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            _ => entity
                .strip_prefix("#x")
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|number| number.parse::<u32>().ok())
                })
                .and_then(char::from_u32),
        };
        if let Some(c) = character {
            output.push(c);
        } else {
            output.push_str(&text[start..=end]);
        }
        cursor = end + 1;
    }
    output.push_str(&text[cursor..]);
    output
}

pub(super) struct SharedFragment<'a> {
    pub start: usize,
    pub end: usize,
    pub xml: &'a str,
    pub body: &'a str,
    pub si: &'a str,
    pub row: usize,
    pub column: usize,
    pub anchor: bool,
}

pub(super) fn shared_formula_fragments(xml: &str) -> Vec<SharedFragment<'_>> {
    if !xml.contains("t=\"shared\"") {
        return Vec::new();
    }
    let Some(root) = super::style_entries::children(xml).into_iter().next() else {
        return Vec::new();
    };
    let Some(data) = super::style_entries::children(root.inner)
        .into_iter()
        .find(|e| e.name == "sheetData")
    else {
        return Vec::new();
    };
    let mut fragments = Vec::new();
    for row in super::style_entries::children(data.inner)
        .into_iter()
        .filter(|e| e.name == "row")
    {
        for cell in super::style_entries::children(row.inner)
            .into_iter()
            .filter(|e| e.name == "c")
        {
            let Some((row_number, column)) =
                super::styles::attr(cell.xml, "r").and_then(parse_address)
            else {
                continue;
            };
            for formula in super::style_entries::children(cell.inner)
                .into_iter()
                .filter(|e| e.name == "f" && super::styles::attr(e.xml, "t") == Some("shared"))
            {
                let Some(si) = super::styles::attr(formula.xml, "si") else {
                    continue;
                };
                let start = formula.xml.as_ptr() as usize - xml.as_ptr() as usize;
                fragments.push(SharedFragment {
                    start,
                    end: start + formula.xml.len(),
                    xml: formula.xml,
                    body: formula.inner,
                    si,
                    row: row_number,
                    column,
                    anchor: super::styles::attr(formula.xml, "ref").is_some(),
                });
            }
        }
    }
    fragments
}

fn materialize_shared_formulas(
    xml: &str,
    mutations: &Mutations<'_>,
    translate: fn(&str, i64, i64) -> Option<String>,
) -> Result<String, String> {
    let fragments = shared_formula_fragments(xml);
    let mut masters = std::collections::HashMap::new();
    for fragment in &fragments {
        if fragment.anchor && !fragment.body.is_empty() {
            let rewritten = mutations
                .get(&fragment.row)
                .and_then(|cols| cols.get(&fragment.column))
                .is_some_and(|mutation| {
                    mutation.fill.is_some()
                        || mutation.edits.iter().any(|edit| {
                            edit.get("writeValue").and_then(Value::as_bool) == Some(true)
                        })
                });
            if rewritten {
                masters.insert(fragment.si, fragment);
            }
        }
    }
    if masters.is_empty() {
        return Ok(xml.to_owned());
    }
    let mut output = String::new();
    let mut cursor = 0;
    for fragment in &fragments {
        let Some(master) = masters.get(fragment.si) else {
            continue;
        };
        let rewritten = mutations
            .get(&fragment.row)
            .and_then(|cols| cols.get(&fragment.column))
            .is_some_and(|mutation| {
                mutation.fill.is_some()
                    || mutation
                        .edits
                        .iter()
                        .any(|edit| edit.get("writeValue").and_then(Value::as_bool) == Some(true))
            });
        if rewritten || fragment.anchor || !fragment.body.is_empty() {
            continue;
        }
        let row = i64::try_from(fragment.row).map_err(|_| "Shared formula row is too large.")?;
        let master_row =
            i64::try_from(master.row).map_err(|_| "Shared formula row is too large.")?;
        let column =
            i64::try_from(fragment.column).map_err(|_| "Shared formula column is too large.")?;
        let master_column =
            i64::try_from(master.column).map_err(|_| "Shared formula column is too large.")?;
        let translated = translate(
            &decode_xml(master.body),
            row - master_row,
            column - master_column,
        );
        output.push_str(&xml[cursor..fragment.start]);
        if let Some(formula) = translated {
            let open_end = fragment.xml.find('>').unwrap_or(fragment.xml.len());
            let mut attributes = fragment.xml[2..open_end].trim_end_matches('/').to_owned();
            for key in ["t", "ref", "si"] {
                if let Some(value) = super::styles::attr(fragment.xml, key) {
                    attributes = attributes.replacen(&format!(" {key}=\"{value}\""), "", 1);
                }
            }
            output.push_str(&format!("<f{attributes}>{}</f>", xml_text(&formula)));
        }
        cursor = fragment.end;
    }
    output.push_str(&xml[cursor..]);
    Ok(output)
}

pub(super) fn infer_addresses(xml: &str) -> Result<String, String> {
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
    let mut body = String::new();
    let mut cursor = 0;
    let mut previous_row = 0;
    for row in super::style_entries::children(data.inner)
        .into_iter()
        .filter(|e| e.name == "row")
    {
        let start = row.xml.as_ptr() as usize - data.inner.as_ptr() as usize;
        body.push_str(&data.inner[cursor..start]);
        let explicit = super::styles::attr(row.xml, "r");
        let row_number = explicit
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(previous_row + 1);
        previous_row = row_number;
        let open_end = row.xml.find('>').ok_or("Malformed worksheet row.")? + 1;
        let open = if explicit.is_none() {
            add_attribute(&row.xml[..open_end], "r", &row_number.to_string())
        } else {
            row.xml[..open_end].to_owned()
        };
        body.push_str(&open);
        let mut cell_cursor = 0;
        let mut next_col = 0;
        for cell in super::style_entries::children(row.inner)
            .into_iter()
            .filter(|e| e.name == "c")
        {
            let offset = cell.xml.as_ptr() as usize - row.inner.as_ptr() as usize;
            body.push_str(&row.inner[cell_cursor..offset]);
            let existing = super::styles::attr(cell.xml, "r");
            let col = existing
                .and_then(parse_address)
                .map(|(_, c)| c)
                .unwrap_or(next_col);
            next_col = col + 1;
            if existing.is_none() {
                body.push_str(&add_attribute(cell.xml, "r", &address(row_number, col)));
            } else {
                body.push_str(cell.xml);
            }
            cell_cursor = offset + cell.xml.len();
        }
        body.push_str(&row.inner[cell_cursor..]);
        if !open.ends_with("/>") {
            body.push_str("</row>");
        }
        cursor = start + row.xml.len();
    }
    body.push_str(&data.inner[cursor..]);
    if data.xml.ends_with("/>") {
        return Ok(xml.to_owned());
    }
    let data_start = data.inner.as_ptr() as usize - xml.as_ptr() as usize;
    Ok(format!(
        "{}{}{}",
        &xml[..data_start],
        body,
        &xml[data_start + data.inner.len()..]
    ))
}

fn expand_dimension(xml: &str, mutations: &Mutations<'_>) -> String {
    let Some(root) = super::style_entries::children(xml).into_iter().next() else {
        return xml.to_owned();
    };
    let dimension = super::style_entries::children(root.inner)
        .into_iter()
        .find(|e| e.name == "dimension");
    let old_ref = dimension.and_then(|d| super::styles::attr(d.xml, "ref"));
    let mut max_row = 1usize;
    let mut max_col = 0usize;
    if let Some(reference) = old_ref {
        for part in reference.split(':') {
            if let Some((row, col)) = parse_address(&part.replace('$', "")) {
                max_row = max_row.max(row);
                max_col = max_col.max(col);
            }
        }
    }
    let target_row = mutations.keys().next_back().copied().unwrap_or(1);
    let target_col = mutations
        .values()
        .filter_map(|cols| cols.keys().next_back().copied())
        .max()
        .unwrap_or(0);
    if old_ref.is_some() && max_row >= target_row && max_col >= target_col {
        return xml.to_owned();
    }
    if old_ref.is_some() {
        max_row = max_row.max(target_row);
        max_col = max_col.max(target_col);
    }
    if let Some(data) = super::style_entries::children(root.inner)
        .into_iter()
        .find(|e| e.name == "sheetData")
    {
        for row in super::style_entries::children(data.inner)
            .into_iter()
            .filter(|e| e.name == "row")
        {
            if old_ref.is_some() {
                if let Some(r) =
                    super::styles::attr(row.xml, "r").and_then(|s| s.parse::<usize>().ok())
                {
                    max_row = max_row.max(r);
                }
            }
            for cell in super::style_entries::children(row.inner)
                .into_iter()
                .filter(|e| e.name == "c")
            {
                if let Some((r, c)) = super::styles::attr(cell.xml, "r").and_then(parse_address) {
                    max_row = max_row.max(r);
                    max_col = max_col.max(c);
                }
            }
        }
    }
    let end = address(max_row, max_col);
    if let Some(dim) = dimension {
        let start = dim.xml.as_ptr() as usize - xml.as_ptr() as usize;
        let first = old_ref
            .unwrap_or("A1")
            .split(':')
            .next()
            .unwrap_or("A1")
            .replace('$', "");
        let replacement = format!("<dimension ref=\"{first}:{end}\"/>");
        format!(
            "{}{}{}",
            &xml[..start],
            replacement,
            &xml[start + dim.xml.len()..]
        )
    } else {
        let end = if end == "A1" {
            end
        } else {
            format!("A1:{end}")
        };
        let insertion = root.inner.as_ptr() as usize - xml.as_ptr() as usize;
        format!(
            "{}<dimension ref=\"{end}\"/>{}",
            &xml[..insertion],
            &xml[insertion..]
        )
    }
}

pub(super) fn transform_cells<'a>(
    xml: &str,
    edits: impl IntoIterator<Item = &'a Value>,
    fills: impl IntoIterator<Item = &'a Value>,
    mut stylesheet: Option<&mut super::styles::StylesheetEditor>,
) -> Result<String, String> {
    let mut mutations: Mutations<'_> = std::collections::BTreeMap::new();
    for fill in fills {
        let start_row = coordinate(fill, "startRow")?;
        let end_row = coordinate(fill, "endRow")?;
        let start_col = coordinate(fill, "startColumn")?;
        let end_col = coordinate(fill, "endColumn")?;
        let value = fill.get("value").ok_or("Bulk fill has no value.")?;
        for row in start_row..=end_row {
            for col in start_col..=end_col {
                mutations
                    .entry(row + 1)
                    .or_default()
                    .entry(col)
                    .or_default()
                    .fill = Some(value);
            }
        }
    }
    for edit in edits {
        let row = coordinate(edit, "row")? + 1;
        let col = coordinate(edit, "column")?;
        mutations
            .entry(row)
            .or_default()
            .entry(col)
            .or_default()
            .edits
            .push(edit);
    }
    if mutations.is_empty() {
        return Ok(xml.to_owned());
    }
    let normalized = infer_addresses(xml)?;
    let normalized = materialize_shared_formulas(
        &normalized,
        &mutations,
        sheet_native::translate_shared_formula,
    )?;
    let xml = normalized.as_str();
    let root = super::style_entries::children(xml)
        .into_iter()
        .next()
        .ok_or("Worksheet has no sheetData element.")?;
    let data = super::style_entries::children(root.inner)
        .into_iter()
        .find(|e| e.name == "sheetData")
        .ok_or("Worksheet has no sheetData element.")?;
    let start = data.xml.as_ptr() as usize - xml.as_ptr() as usize;
    let open_end = data
        .xml
        .find('>')
        .ok_or("Worksheet has no sheetData element.")?
        + 1;
    let self_closing = data.xml[..open_end].ends_with("/>");
    let mut output = String::with_capacity(xml.len());
    output.push_str(&xml[..start]);
    if self_closing {
        output.push_str(&data.xml[..open_end - 2]);
        output.push('>');
    } else {
        output.push_str(&data.xml[..open_end]);
    }
    let mut pending = mutations.iter().peekable();
    let mut previous_row = 0;
    let mut cursor = 0;
    for row_element in super::style_entries::children(data.inner)
        .into_iter()
        .filter(|e| e.name == "row")
    {
        let offset = row_element.xml.as_ptr() as usize - data.inner.as_ptr() as usize;
        output.push_str(&data.inner[cursor..offset]);
        let explicit = super::styles::attr(row_element.xml, "r")
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0);
        let row = explicit.unwrap_or(previous_row + 1);
        previous_row = row;
        while pending.peek().is_some_and(|(key, _)| **key < row) {
            if let Some((key, items)) = pending.next() {
                output.push_str(&build_row(*key, items, &mut stylesheet)?);
            }
        }
        if pending.peek().is_some_and(|(key, _)| **key == row) {
            if let Some((_, items)) = pending.next() {
                output.push_str(&edit_row(row_element.xml, row, items, &mut stylesheet)?);
            }
        } else if explicit.is_none() {
            output.push_str(&add_attribute(row_element.xml, "r", &row.to_string()));
        } else {
            output.push_str(row_element.xml);
        }
        cursor = offset + row_element.xml.len();
    }
    output.push_str(&data.inner[cursor..]);
    for (row, items) in pending {
        output.push_str(&build_row(*row, items, &mut stylesheet)?);
    }
    output.push_str("</sheetData>");
    output.push_str(&xml[start + data.xml.len()..]);
    Ok(expand_dimension(&output, &mutations))
}

pub(super) fn mark_spill_anchors<'a>(
    xml: &str,
    edits: impl IntoIterator<Item = &'a Value>,
    cm: usize,
) -> Result<String, String> {
    let mut addresses = std::collections::HashSet::new();
    for edit in edits {
        if edit.get("writeValue").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        if edit
            .get("cell")
            .and_then(|c| c.get("formula"))
            .and_then(Value::as_str)
            .is_some_and(spills_dynamic_array)
        {
            addresses.insert(address(
                coordinate(edit, "row")? + 1,
                coordinate(edit, "column")?,
            ));
        }
    }
    if addresses.is_empty() {
        return Ok(xml.to_owned());
    }
    let Some(root) = super::style_entries::children(xml).into_iter().next() else {
        return Ok(xml.to_owned());
    };
    let Some(data) = super::style_entries::children(root.inner)
        .into_iter()
        .find(|e| e.name == "sheetData")
    else {
        return Ok(xml.to_owned());
    };
    let mut output = String::new();
    let mut cursor = 0;
    for row in super::style_entries::children(data.inner)
        .into_iter()
        .filter(|e| e.name == "row")
    {
        for cell in super::style_entries::children(row.inner)
            .into_iter()
            .filter(|e| e.name == "c")
        {
            if !super::styles::attr(cell.xml, "r").is_some_and(|r| addresses.contains(r))
                || super::styles::attr(cell.xml, "cm").is_some()
            {
                continue;
            }
            let at = cell.xml.as_ptr() as usize - xml.as_ptr() as usize;
            let open_end = cell.xml.find('>').ok_or("Malformed worksheet cell.")?;
            let open = &cell.xml[..open_end];
            let insert = if open.ends_with('/') {
                open_end - 1
            } else {
                open_end
            };
            output.push_str(&xml[cursor..at + insert]);
            output.push_str(&format!(" cm=\"{cm}\""));
            cursor = at + insert;
        }
    }
    if cursor == 0 {
        return Ok(xml.to_owned());
    }
    output.push_str(&xml[cursor..]);
    Ok(output)
}

pub(super) fn patch_cached_formula_values(xml: &str, cells: &[Value]) -> Result<String, String> {
    let mut values = std::collections::HashMap::new();
    for cell in cells {
        let row = coordinate(cell, "row")? + 1;
        let col = coordinate(cell, "column")?;
        values.insert(
            (row, col),
            cell.get("value").ok_or("Formula cache value is missing.")?,
        );
    }
    if values.is_empty() {
        return Ok(xml.to_owned());
    }
    let normalized = infer_addresses(xml)?;
    let Some(root) = super::style_entries::children(&normalized)
        .into_iter()
        .next()
    else {
        return Ok(normalized);
    };
    let Some(data) = super::style_entries::children(root.inner)
        .into_iter()
        .find(|e| e.name == "sheetData")
    else {
        return Ok(normalized);
    };
    let mut output = String::new();
    let mut cursor = 0;
    for row in super::style_entries::children(data.inner)
        .into_iter()
        .filter(|e| e.name == "row")
    {
        for cell in super::style_entries::children(row.inner)
            .into_iter()
            .filter(|e| e.name == "c")
        {
            let Some(reference) = super::styles::attr(cell.xml, "r") else {
                continue;
            };
            let Some(value) = parse_address(reference).and_then(|position| values.get(&position))
            else {
                continue;
            };
            let child_elements = super::style_entries::children(cell.inner);
            if !child_elements.iter().any(|e| e.name == "f") {
                continue;
            }
            let mut body = String::new();
            let mut body_cursor = 0;
            for child in child_elements.iter().filter(|e| e.name == "v") {
                let at = child.xml.as_ptr() as usize - cell.inner.as_ptr() as usize;
                body.push_str(&cell.inner[body_cursor..at]);
                body_cursor = at + child.xml.len();
            }
            body.push_str(&cell.inner[body_cursor..]);
            let (kind, cached) = match value {
                Value::Null => ("", String::new()),
                Value::Bool(value) => {
                    (" t=\"b\"", format!("<v>{}</v>", if *value { 1 } else { 0 }))
                }
                Value::Number(value) => {
                    let formatted = if value.is_f64() {
                        jscompat::number_to_string(
                            value.as_f64().ok_or("Invalid formula cache number.")?,
                        )
                    } else {
                        value.to_string()
                    };
                    ("", format!("<v>{formatted}</v>"))
                }
                Value::String(value) if value.is_empty() => ("", String::new()),
                Value::String(value) => (" t=\"str\"", format!("<v>{}</v>", cell_text(value))),
                Value::Object(_) => (
                    " t=\"e\"",
                    format!(
                        "<v>{}</v>",
                        cell_text(
                            value
                                .get("error")
                                .and_then(Value::as_str)
                                .ok_or("Invalid formula error value.")?
                        )
                    ),
                ),
                _ => return Err("Invalid formula cache value.".into()),
            };
            let opening = cell
                .xml
                .split('>')
                .next()
                .ok_or("Malformed formula cell.")?;
            let mut attrs = format!(" {}", opening.strip_prefix("<c").unwrap_or("").trim());
            for name in ["r", "t"] {
                if let Some(value) = super::styles::attr(cell.xml, name) {
                    attrs = attrs.replacen(&format!(" {name}=\"{value}\""), "", 1);
                }
            }
            let attrs = attrs.split_whitespace().collect::<Vec<_>>().join(" ");
            let attrs = if attrs.is_empty() {
                String::new()
            } else {
                format!(" {attrs}")
            };
            let at = cell.xml.as_ptr() as usize - normalized.as_ptr() as usize;
            output.push_str(&normalized[cursor..at]);
            output.push_str(&format!(
                "<c r=\"{reference}\"{attrs}{kind}>{body}{cached}</c>"
            ));
            cursor = at + cell.xml.len();
        }
    }
    if cursor == 0 {
        return Ok(normalized);
    }
    output.push_str(&normalized[cursor..]);
    Ok(output)
}

pub(super) fn apply_cell_edits(
    state: &mut super::state::PlannerState,
    request: &Value,
    mut stylesheet: Option<&mut super::styles::StylesheetEditor>,
    dynamic_cm: Option<usize>,
) -> Result<(), String> {
    let empty = Vec::new();
    let edits = request
        .get("edits")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let fills = request
        .get("bulkConstantFills")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in fills.iter().chain(edits.iter()) {
        let name = item
            .get("sheetName")
            .and_then(Value::as_str)
            .ok_or("Cell mutation has no sheetName.")?;
        if seen.insert(name) {
            names.push(name);
        }
    }
    for name in names {
        let sheet = state
            .worksheet_mut(name)
            .ok_or_else(|| format!("Sheet \"{name}\" was not loaded."))?;
        let sheet_edits = edits
            .iter()
            .filter(|edit| edit.get("sheetName").and_then(Value::as_str) == Some(name));
        let sheet_fills = fills
            .iter()
            .filter(|fill| fill.get("sheetName").and_then(Value::as_str) == Some(name));
        let mut xml = transform_cells(
            &sheet.xml,
            sheet_edits,
            sheet_fills,
            stylesheet.as_deref_mut(),
        )?;
        if let Some(cm) = dynamic_cm {
            xml = mark_spill_anchors(
                &xml,
                edits
                    .iter()
                    .filter(|edit| edit.get("sheetName").and_then(Value::as_str) == Some(name)),
                cm,
            )?;
        }
        sheet.xml = std::sync::Arc::new(xml);
    }
    if let Some(sheets) = request.get("formulaValues").and_then(Value::as_array) {
        for entry in sheets {
            let name = entry
                .get("sheetName")
                .and_then(Value::as_str)
                .ok_or("Formula cache has no sheetName.")?;
            let cells = entry
                .get("cells")
                .and_then(Value::as_array)
                .ok_or("Formula cache has no cells.")?;
            if cells.is_empty() {
                continue;
            }
            let sheet = state
                .worksheet_mut(name)
                .ok_or_else(|| format!("Sheet \"{name}\" was not loaded."))?;
            sheet.xml = std::sync::Arc::new(patch_cached_formula_values(&sheet.xml, cells)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_style_on_clear_and_escapes_literal_xlsx_sequences() {
        assert_eq!(
            serialize_styled_cell("A1", &json!({"value": null}), Some("4"), None, None).unwrap(),
            "<c r=\"A1\" s=\"4\"/>"
        );
        assert_eq!(
            serialize_styled_cell("B2", &json!({"value": "_x000D_\r&"}), None, None, None).unwrap(),
            "<c r=\"B2\" t=\"inlineStr\"><is><t xml:space=\"preserve\">_x005F_x000D__x000D_&amp;</t></is></c>"
        );
    }

    #[test]
    fn existing_array_extent_beats_spill_guess() {
        assert_eq!(
            serialize_styled_cell(
                "A1",
                &json!({"value": null, "formula": "=SORT(B1:B5)"}),
                None,
                None,
                Some("A1:A5")
            )
            .unwrap(),
            "<c r=\"A1\"><f t=\"array\" ref=\"A1:A5\">_xlfn._xlws.SORT(B1:B5)</f></c>"
        );
    }
    #[test]
    fn applies_fills_before_edits_and_inserts_in_order() {
        let xml = "<worksheet><sheetData><row r=\"2\"><c r=\"B2\" s=\"3\"><v>1</v></c><c r=\"D2\"><v>9</v></c></row></sheetData></worksheet>";
        let fills = [json!({"startRow":1,"endRow":1,"startColumn":0,"endColumn":2,"value":5})];
        let edits = [json!({"row":1,"column":1,"writeValue":true,"cell":{"value":"edited"}})];
        let output = transform_cells(xml, &edits, &fills, None).unwrap();
        assert!(output.contains("<row r=\"2\"><c r=\"A2\"><v>5</v></c><c r=\"B2\" s=\"3\" t=\"inlineStr\"><is><t xml:space=\"preserve\">edited</t></is></c><c r=\"C2\"><v>5</v></c><c r=\"D2\"><v>9</v></c></row>"));
    }
    #[test]
    fn editing_shared_master_materializes_followers() {
        let xml = "<worksheet><sheetData><row r=\"1\"><c r=\"A1\"><f t=\"shared\" si=\"0\" ref=\"A1:A2\">B1+1</f><v>5</v></c></row><row r=\"2\"><c r=\"A2\"><f t=\"shared\" si=\"0\"/><v>6</v></c></row></sheetData></worksheet>";
        let edits = [json!({"row":0,"column":0,"writeValue":true,"cell":{"value":42}})];
        let output = transform_cells(xml, &edits, &[], None).unwrap();
        assert!(output.contains("<c r=\"A1\"><v>42</v></c>"));
        assert!(output.contains("<c r=\"A2\"><f>B2+1</f><v>6</v></c>"));
    }
    #[test]
    fn cache_refresh_keeps_formula_and_replaces_only_cached_value() {
        let xml = "<worksheet><sheetData><row r=\"1\"><c r=\"A1\" s=\"3\" t=\"str\"><f>1+1</f><v>old</v></c><c r=\"B1\"><v>7</v></c></row></sheetData></worksheet>";
        let output = patch_cached_formula_values(
            xml,
            &[
                json!({"row":0,"column":0,"value":2}),
                json!({"row":0,"column":1,"value":9}),
            ],
        )
        .unwrap();
        assert!(output.contains("<c r=\"A1\" s=\"3\"><f>1+1</f><v>2</v></c>"));
        assert!(output.contains("<c r=\"B1\"><v>7</v></c>"));
    }
}
