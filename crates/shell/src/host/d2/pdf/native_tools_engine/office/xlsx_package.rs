//! OOXML spreadsheet package assembly.
use super::xlsx_sheet::Sheet;
use super::xlsx_styles::{StylePool, escape};
use std::io::{Cursor, Write};
const DECL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>";
pub(super) fn build(sheets: &[Sheet], styles: &StylePool) -> Result<Vec<u8>, String> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut file = |name: &str, content: String| -> Result<(), String> {
        zip.start_file(name, options).map_err(|e| e.to_string())?;
        zip.write_all(content.as_bytes()).map_err(|e| e.to_string())
    };
    let overrides=(1..=sheets.len()).map(|i|format!("<Override PartName=\"/xl/worksheets/sheet{i}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>")).collect::<String>();
    file(
        "[Content_Types].xml",
        format!(
            "{DECL}<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>{overrides}<Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/></Types>"
        ),
    )?;
    file(
        "_rels/.rels",
        format!(
            "{DECL}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>"
        ),
    )?;
    let entries = sheets
        .iter()
        .enumerate()
        .map(|(i, s)| {
            format!(
                "<sheet name=\"{}\" sheetId=\"{}\" r:id=\"rId{}\"/>",
                escape(&s.name),
                i + 1,
                i + 1
            )
        })
        .collect::<String>();
    file(
        "xl/workbook.xml",
        format!(
            "{DECL}<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets>{entries}</sheets></workbook>"
        ),
    )?;
    let relations=(1..=sheets.len()).map(|i|format!("<Relationship Id=\"rId{i}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{i}.xml\"/>")).collect::<String>();
    file(
        "xl/_rels/workbook.xml.rels",
        format!(
            "{DECL}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{relations}<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/></Relationships>",
            sheets.len() + 1
        ),
    )?;
    file("xl/styles.xml", styles.xml())?;
    for (i, sheet) in sheets.iter().enumerate() {
        file(&format!("xl/worksheets/sheet{}.xml", i + 1), sheet.xml())?;
    }
    drop(file);
    Ok(zip.finish().map_err(|e| e.to_string())?.into_inner())
}
#[cfg(test)]
mod tests {
    use super::super::xlsx_sheet::{Cell, Value};
    use super::*;
    use std::io::Read;
    #[test]
    fn workbook_package_contains_editable_typed_cells() {
        let sheet = Sheet {
            name: "Page 1".into(),
            cells: vec![
                Cell {
                    row: 0,
                    col: 0,
                    value: Some(Value::Number(12.5)),
                    style: 0,
                },
                Cell {
                    row: 0,
                    col: 1,
                    value: Some(Value::Text("A&B".into())),
                    style: 0,
                },
            ],
            ..Sheet::default()
        };
        let bytes = build(&[sheet], &StylePool::default()).unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut xml = String::new();
        zip.by_name("xl/worksheets/sheet1.xml")
            .unwrap()
            .read_to_string(&mut xml)
            .unwrap();
        assert!(xml.contains("<v>12.5</v>"));
        assert!(xml.contains("A&amp;B"));
    }
}
