//! Mail-merge row reader matching xlsx-gateway's readSheetRows.
use std::collections::HashSet;

use file_parse::xlsx_xml_scan::{Markup, tokens};
use jscompat::{is_js_space, js_regex, math_round, number_to_string, to_number, try_js_regex};
use ooxml_pkg::{Package, ZipLimits, load_zip_package_staged};

fn attr<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
    let mut start = 0;
    while let Some(relative) = attributes[start..].find(name) {
        let at = start + relative;
        let before = at == 0
            || attributes[..at]
                .chars()
                .next_back()
                .is_some_and(is_js_space);
        let tail = &attributes[at + name.len()..];
        if before
            && let Some(value) = tail.strip_prefix("=\"")
            && let Some(end) = value.find('"')
        {
            return Some(&value[..end]);
        }
        start = at + name.len();
    }
    None
}

fn xml_text(text: &str) -> String {
    js_regex!(r"&(?:#x([0-9A-Fa-f]+)|#([0-9]+)|(quot|apos|lt|gt|amp));").replace_all(text, |m| {
        if let Some(name) = m.get(3) {
            return match name {
                "quot" => "\"",
                "apos" => "'",
                "lt" => "<",
                "gt" => ">",
                _ => "&",
            }
            .to_owned();
        }
        let number = if let Some(hex) = m.get(1) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            m.get(2).and_then(|n| n.parse().ok())
        };
        number
            .and_then(char::from_u32)
            .map(|c| c.to_string())
            .unwrap_or_else(|| m.as_str().to_owned())
    })
}

fn cell_text(text: &str) -> String {
    let text = xml_text(text).replace("\r\n", "\n").replace('\r', "\n");
    let text = js_regex!(r"_x([0-9A-Fa-f]{4})_").replace_all(&text, |m| {
        u32::from_str_radix(m.get(1).unwrap_or_default(), 16)
            .ok()
            .and_then(char::from_u32)
            .map(|c| c.to_string())
            .unwrap_or_else(|| m.as_str().to_owned())
    });
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn part<'a>(zip: &'a Package, path: &str) -> Result<&'a str, String> {
    zip.text(path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Missing XLSX part: {path}"))
}

fn relationship_target(target: &str) -> Result<String, String> {
    let invalid = || "Invalid OPC relationship target.".to_owned();
    let without_fragment = target.trim().split('#').next().unwrap_or_default();
    if without_fragment.is_empty() {
        return Err(invalid());
    }
    let bytes = without_fragment.as_bytes();
    for (i, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && (i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit())
        {
            return Err(invalid());
        }
    }
    let decoded = percent_encoding::percent_decode_str(without_fragment)
        .decode_utf8()
        .map_err(|_| invalid())?;
    let s = decoded.as_ref();
    if s.is_empty()
        || s.contains('\0')
        || s.starts_with("//")
        || s.starts_with('\\')
        || js_regex!(r"^[A-Za-z][A-Za-z0-9+.-]*:").test(s)
    {
        return Err(invalid());
    }
    let replaced = s.replace('\\', "/");
    let mut segments = if s.starts_with('/') {
        Vec::new()
    } else {
        vec!["xl"]
    };
    for segment in replaced.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return Err(invalid());
                }
            }
            value => segments.push(value),
        }
    }
    if segments.is_empty() {
        return Err(invalid());
    }
    Ok(segments.join("/"))
}

struct Child<'a> {
    name: &'a str,
    opening: &'a str,
    inner: &'a str,
}
fn children(xml: &str) -> Vec<Child<'_>> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut open = None;
    for token in tokens(xml) {
        let Markup::Tag {
            closing,
            name,
            attributes,
        } = token.markup
        else {
            continue;
        };
        if closing {
            if depth == 0 {
                break;
            }
            depth -= 1;
            if depth == 0
                && let Some((name, start, end)) = open.take()
            {
                out.push(Child {
                    name,
                    opening: &xml[start..end],
                    inner: &xml[end..token.start],
                });
            }
        } else if attributes.ends_with('/') {
            if depth == 0 {
                out.push(Child {
                    name,
                    opening: &xml[token.start..token.end],
                    inner: "",
                });
            }
        } else {
            if depth == 0 {
                open = Some((name, token.start, token.end));
            }
            depth += 1;
        }
    }
    out
}
fn local(name: &str) -> &str {
    name.split_once(':').map_or(name, |(_, name)| name)
}
fn style_entries<'a>(xml: &'a str, tag: &str, out: &mut Vec<&'a str>) {
    for child in children(xml) {
        if child.name == tag {
            out.push(child.opening);
        } else if local(child.name) == "AlternateContent" {
            let branches = children(child.inner);
            if let Some(branch) = branches
                .iter()
                .find(|b| local(b.name) == "Fallback")
                .or_else(|| branches.iter().find(|b| local(b.name) == "Choice"))
            {
                style_entries(branch.inner, tag, out);
            }
        }
    }
}
fn section<'a>(xml: &'a str, name: &str) -> Result<&'a str, String> {
    let found = match name {
        "numFmts" => try_js_regex!(r"<numFmts\b[^>]*>([\s\S]*?)</numFmts>")
            .try_find_iter(xml)
            .next(),
        _ => try_js_regex!(r"<cellXfs\b[^>]*>([\s\S]*?)</cellXfs>")
            .try_find_iter(xml)
            .next(),
    };
    Ok(found
        .transpose()
        .map_err(|e| e.to_string())?
        .and_then(|m| m.get(1))
        .unwrap_or(""))
}
fn date_styles(xml: &str) -> Result<HashSet<usize>, String> {
    let mut entries = Vec::new();
    style_entries(section(xml, "numFmts")?, "numFmt", &mut entries);
    let mut custom = HashSet::new();
    for entry in entries {
        let id = attr(entry, "numFmtId").map(to_number).unwrap_or(f64::NAN);
        let code = xml_text(attr(entry, "formatCode").unwrap_or(""));
        let code = js_regex!(r#"\"[^\"]*\""#).replace_all(&code, |_| String::new());
        let code = js_regex!(r"\[[^\]]*\]").replace_all(&code, |_| String::new());
        let code = js_regex!(r"\\.").replace_all(&code, |_| String::new());
        let has_date = js_regex!(r"[ymdh]", "i").test(&code);
        if (has_date || (!js_regex!(r"[#0?@]").test(&code) && code.contains('s'))) && id.is_finite()
        {
            custom.insert(if id == 0.0 {
                0.0f64.to_bits()
            } else {
                id.to_bits()
            });
        }
    }
    let builtin = [
        14, 15, 16, 17, 18, 19, 20, 21, 22, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 45, 46, 47, 50,
        51, 52, 53, 54, 55, 56, 57, 58,
    ];
    let mut xfs = Vec::new();
    style_entries(section(xml, "cellXfs")?, "xf", &mut xfs);
    Ok(xfs
        .iter()
        .enumerate()
        .filter_map(|(i, entry)| {
            let id = attr(entry, "numFmtId").map(to_number).unwrap_or(f64::NAN);
            let key = if id == 0.0 {
                0.0f64.to_bits()
            } else {
                id.to_bits()
            };
            (id.is_finite()
                && ((id.fract() == 0.0 && builtin.contains(&(id as i64))) || custom.contains(&key)))
            .then_some(i)
        })
        .collect())
}
fn serial_text(serial: f64, date1904: bool) -> String {
    let offset = if date1904 {
        -2_082_844_800_000.0
    } else {
        -2_209_161_600_000.0
    };
    let ms = offset
        + math_round((serial + if !date1904 && serial < 60.0 { 1.0 } else { 0.0 }) * 86_400_000.0);
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        return number_to_string(serial);
    }
    let Some(date) = chrono::DateTime::from_timestamp_millis(ms as i64) else {
        return number_to_string(serial);
    };
    let iso = date.format("%Y-%m-%dT%H:%M:%S").to_string();
    if iso.len() < 19 {
        return number_to_string(serial);
    }
    let day = &iso[..10];
    let time = &iso[11..19];
    if serial < 1.0 {
        time.to_owned()
    } else if time == "00:00:00" {
        day.to_owned()
    } else {
        format!("{day} {time}")
    }
}

fn text_runs(body: &str) -> Result<String, String> {
    let mut out = String::new();
    for found in try_js_regex!(r"<t(?:\s[^>]*)?>([\s\S]*?)</t>").try_find_iter(body) {
        out.push_str(&cell_text(
            found.map_err(|e| e.to_string())?.get(1).unwrap_or(""),
        ));
    }
    Ok(out)
}

pub(super) fn read(bytes: &[u8]) -> Result<Vec<Vec<String>>, String> {
    let limit = ZipLimits {
        max_parts: 10_000,
        max_part_bytes: 268_435_456.0,
        max_total_bytes: 268_435_456.0,
    };
    let zip = load_zip_package_staged(bytes, &limit, false).map_err(|e| e.into_message())?;
    let workbook = part(&zip, "xl/workbook.xml")?;
    let first = try_js_regex!(r#"<sheet\b((?:\"[^\"]*\"|'[^']*'|[^>\"'])*?)/?>"#)
        .try_find_iter(workbook)
        .next()
        .transpose()
        .map_err(|e| e.to_string())?
        .and_then(|m| attr(m.get(1).unwrap_or(""), "name"))
        .filter(|n| !n.is_empty())
        .ok_or_else(|| "Workbook contains no readable worksheets.".to_owned())?;
    let name = xml_text(first);
    let sheets = file_parse::xlsx::workbook_sheet_elements(workbook);
    let sheet = sheets
        .iter()
        .find(|attrs| attrs.iter().any(|(k, v)| k == "name" && v == &name))
        .ok_or_else(|| format!("Sheet \"{name}\" was not found in workbook.xml."))?;
    let id = sheet
        .iter()
        .find(|(k, _)| k == "r:id")
        .or_else(|| {
            sheet
                .iter()
                .find(|(k, _)| js_regex!(r"^[A-Za-z_][\w.-]*:id$").test(k))
        })
        .map(|(_, v)| v.as_str())
        .ok_or_else(|| format!("Sheet \"{name}\" was not found in workbook.xml."))?;
    let rels = part(&zip, "xl/_rels/workbook.xml.rels")?;
    let mut target = None;
    for rel in js_regex!(r"<Relationship\b[^>]*/?>").find_iter(rels) {
        let xml = rel.as_str();
        if attr(xml, "Id") == Some(id) {
            target = attr(xml, "Target");
            break;
        }
    }
    let target = xml_text(target.ok_or_else(|| format!("Relationship {id} was not found."))?);
    let target = target
        .strip_prefix("/xl/")
        .or_else(|| target.strip_prefix("xl/"))
        .unwrap_or(&target);
    let worksheet = part(&zip, &relationship_target(target)?)?;
    let mut shared = Vec::new();
    if let Some(xml) = zip
        .text("xl/sharedStrings.xml")
        .map_err(|e| e.to_string())?
    {
        for item in try_js_regex!(r"<si(?:\s[^>]*)?>([\s\S]*?)</si>").try_find_iter(xml) {
            shared.push(text_runs(
                item.map_err(|e| e.to_string())?.get(1).unwrap_or(""),
            )?);
        }
    }
    let styles = zip
        .text("xl/styles.xml")
        .map_err(|e| e.to_string())?
        .map(date_styles)
        .transpose()?
        .unwrap_or_default();
    let date1904 = js_regex!(r#"<workbookPr\b[^>]*\bdate1904=\"(?:1|true)\""#).test(workbook);
    let mut rows = Vec::new();
    for row in try_js_regex!(r"<row\b([^>]*)>([\s\S]*?)</row>").try_find_iter(worksheet) {
        let row = row.map_err(|e| e.to_string())?;
        let n = attr(row.get(1).unwrap_or(""), "r")
            .map(to_number)
            .unwrap_or(f64::NAN);
        let index = if n.is_finite() && n > 0.0 && n.fract() == 0.0 {
            (n - 1.0) as usize
        } else {
            rows.len()
        };
        let mut cells = Vec::new();
        for cell in try_js_regex!(r"<c\b([^>]*?)(?:/>|>([\s\S]*?)</c>)")
            .try_find_iter(row.get(2).unwrap_or(""))
        {
            let cell = cell.map_err(|e| e.to_string())?;
            let a = cell.get(1).unwrap_or("");
            let col = match attr(a, "r").filter(|address| !address.is_empty()) {
                None => cells.len(),
                Some(address) => {
                    let Some(m) = js_regex!(r"^\$?([A-Z]+)\$?[0-9]+$").exec(address) else {
                        continue;
                    };
                    let mut col = 0usize;
                    for b in m.get(1).unwrap_or("").bytes() {
                        col = col
                            .saturating_mul(26)
                            .saturating_add(usize::from(b - b'A' + 1));
                    }
                    col.saturating_sub(1)
                }
            };
            let body = cell.get(2).unwrap_or("");
            let ty = attr(a, "t");
            let raw = try_js_regex!(r"<v(?:\s[^>]*)?>([\s\S]*?)</v>")
                .try_find_iter(body)
                .next()
                .transpose()
                .map_err(|e| e.to_string())?
                .and_then(|m| m.get(1));
            let value = if ty == Some("inlineStr") {
                text_runs(body)?
            } else if let Some(raw) = raw {
                match ty {
                    Some("s") => {
                        let n = to_number(raw);
                        if n.is_finite() && n >= 0.0 && n.fract() == 0.0 {
                            shared.get(n as usize).cloned().unwrap_or_default()
                        } else {
                            String::new()
                        }
                    }
                    Some("str") => cell_text(raw),
                    Some("b") => {
                        if raw == "1" {
                            "TRUE".into()
                        } else {
                            "FALSE".into()
                        }
                    }
                    _ => {
                        let text = cell_text(raw);
                        let style = attr(a, "s").map(to_number).unwrap_or(f64::NAN);
                        let serial = to_number(&text);
                        if (ty.is_none() || ty == Some("n"))
                            && style.is_finite()
                            && style >= 0.0
                            && style.fract() == 0.0
                            && styles.contains(&(style as usize))
                            && serial.is_finite()
                        {
                            serial_text(serial, date1904)
                        } else {
                            text
                        }
                    }
                }
            } else {
                String::new()
            };
            if col >= cells.len() {
                cells.resize(col + 1, String::new());
            }
            cells[col] = value;
        }
        if index >= rows.len() {
            rows.resize(index + 1, Vec::new());
        }
        rows[index] = cells;
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::{ZipWriter, write::SimpleFileOptions};

    fn workbook(date1904: bool) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let parts = [
            ("xl/workbook.xml", format!(r#"<workbook xmlns:r="urn:rel"><workbookPr date1904="{}"/><sheets><sheet name="People" r:id="rId1"/></sheets></workbook>"#, if date1904 { "1" } else { "0" })),
            ("xl/_rels/workbook.xml.rels", r#"<Relationships><Relationship Target="worksheets/sheet1.xml" Id="rId1"/></Relationships>"#.to_owned()),
            ("xl/sharedStrings.xml", r#"<sst><si><r><t>A&amp;</t></r><r><t>B_x000D_</t></r></si></sst>"#.to_owned()),
            ("xl/styles.xml", r#"<styleSheet xmlns:mc="urn:mc"><numFmts><numFmt numFmtId="164" formatCode="yyyy-mm-dd"/></numFmts><cellXfs><xf numFmtId="0"/><mc:AlternateContent><mc:Choice><xf numFmtId="0"/></mc:Choice><mc:Fallback><xf numFmtId="164"/></mc:Fallback></mc:AlternateContent></cellXfs></styleSheet>"#.to_owned()),
            ("xl/worksheets/sheet1.xml", r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="C1" t="b"><v>1</v></c></row><row r="3"><c r="A3" s="1"><f>2+2</f><v>1.5</v></c><c r="B3" t="inlineStr"><is><t>Hi</t><r><t> there</t></r></is></c></row></sheetData></worksheet>"#.to_owned()),
        ];
        for (name, xml) in parts {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(xml.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn reads_cached_values_and_sparse_rows_in_1900_system() {
        assert_eq!(
            read(&workbook(false)).unwrap(),
            vec![
                vec!["A&B\n", "", "TRUE"],
                vec![],
                vec!["1900-01-01 12:00:00", "Hi there"],
            ]
        );
    }

    #[test]
    fn respects_1904_epoch_and_relationship_safety() {
        assert_eq!(read(&workbook(true)).unwrap()[2][0], "1904-01-02 12:00:00");
        assert!(relationship_target("%ZZ").is_err());
        assert!(relationship_target("../../escape.xml").is_err());
    }
}
