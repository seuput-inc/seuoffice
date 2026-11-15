use std::fs::File;
use std::io::{Cursor,Write};
use shell_ipc_d1::sheets::engine::{EngineResult,engine_error};
use super::scanner::Sheet;
pub(super) use super::super::workbook_csv_xlsx::numeric;
use super::super::workbook_csv_xlsx::{label,quoted_text,xlsx_text};
const TEMPLATE:&[u8]=include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/../../apps/shell/build/shell-new/blank.xlsx"));
fn xml(s:&str)->String{s.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;").replace('"',"&quot;")}
fn err(e:impl std::fmt::Display)->shell_contract::IpcError{engine_error(e.to_string())}
fn worksheet(sheet:&Sheet,writer:&mut impl Write)->EngineResult<()> {
    let mut cols=sheet.rows.iter().map(Vec::len).max().unwrap_or(0).max(1);let mut rows=sheet.rows.len().max(1);
    for &(_,_,r,c) in &sheet.merges {cols=cols.max(c+1);rows=rows.max(r+1);}
    write!(writer,"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><dimension ref=\"A1:{}{}\"/><sheetData>",label(cols-1),rows).map_err(err)?;
    for (row_index,row) in sheet.rows.iter().enumerate() {
        let mut started=false;
        for (col,value) in row.iter().enumerate() {
            if value.is_empty(){continue;}if !started {write!(writer,"<row r=\"{}\">",row_index+1).map_err(err)?;started=true;}
            let reference=format!("{}{}",label(col),row_index+1);let quoted=quoted_text(value);
            if quoted.is_none() && let Some(number)=numeric(value) {write!(writer,"<c r=\"{reference}\"><v>{}</v></c>",jscompat::number_to_string(number)).map_err(err)?;}
            else {write!(writer,"<c r=\"{reference}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>",xlsx_text(quoted.as_deref().unwrap_or(value))).map_err(err)?;}
        }
        if started {writer.write_all(b"</row>").map_err(err)?;}
    }
    writer.write_all(b"</sheetData>").map_err(err)?;
    if !sheet.merges.is_empty() {write!(writer,"<mergeCells count=\"{}\">",sheet.merges.len()).map_err(err)?;for &(r,c,r2,c2) in &sheet.merges {write!(writer,"<mergeCell ref=\"{}{}:{}{}\"/>",label(c),r+1,label(c2),r2+1).map_err(err)?;}writer.write_all(b"</mergeCells>").map_err(err)?;}
    writer.write_all(b"</worksheet>").map_err(err)
}
pub(super) fn write(path:&str,sheets:&[Sheet])->EngineResult<()> {
    let output=File::create(path).map_err(err)?;let mut zip=zip::ZipWriter::new(output);
    let opts=zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let overrides=sheets.iter().enumerate().map(|(i,_)|format!("<Override PartName=\"/xl/worksheets/sheet{}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>",i+1)).collect::<String>();
    let types=format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>{overrides}<Override PartName=\"/xl/theme/theme1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.theme+xml\"/><Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/></Types>");
    let rels="<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>";
    let entries=sheets.iter().enumerate().map(|(i,s)|format!("<sheet name=\"{}\" sheetId=\"{}\" r:id=\"rId{}\"/>",xml(&s.name),i+1,i+1)).collect::<String>();
    let workbook=format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets>{entries}</sheets></workbook>");
    let relationships=sheets.iter().enumerate().map(|(i,_)|format!("<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{}.xml\"/>",i+1,i+1)).collect::<String>();
    let workbook_rels=format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{relationships}<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme\" Target=\"theme/theme1.xml\"/><Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/></Relationships>",sheets.len()+1,sheets.len()+2);
    for (name,content) in [("[Content_Types].xml",types.as_str()),("_rels/.rels",rels),("xl/workbook.xml",workbook.as_str()),("xl/_rels/workbook.xml.rels",workbook_rels.as_str())] {zip.start_file(name,opts).map_err(err)?;zip.write_all(content.as_bytes()).map_err(err)?;}
    let mut template=zip::ZipArchive::new(Cursor::new(TEMPLATE)).map_err(err)?;
    for name in ["xl/theme/theme1.xml","xl/styles.xml"] {let mut source=template.by_name(name).map_err(err)?;zip.start_file(name,opts).map_err(err)?;std::io::copy(&mut source,&mut zip).map_err(err)?;}
    for (i,sheet) in sheets.iter().enumerate(){zip.start_file(format!("xl/worksheets/sheet{}.xml",i+1),opts).map_err(err)?;worksheet(sheet,&mut zip)?;}
    zip.finish().map_err(err)?;Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn merges_extend_dimension_and_values_keep_type(){
        let sheet=Sheet{name:"Test".into(),rows:vec![vec!["001".into(),"2.5".into()]],merges:vec![(0,0,2,2)]};let mut out=Vec::new();worksheet(&sheet,&mut out).unwrap();let out=String::from_utf8(out).unwrap();assert!(out.contains("dimension ref=\"A1:C3\""));assert!(out.contains("t=\"inlineStr\""));assert!(out.contains("<v>2.5</v>"));assert!(out.contains("mergeCell ref=\"A1:C3\""));
    }
    #[test] fn multi_sheet_package_retains_names_values_and_merges() {
        use std::io::Read;
        let unique=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!("legacy-html-{}-{unique}.xlsx",std::process::id()));
        let sheets=vec![
            Sheet{name:"학교".into(),rows:vec![vec!["한글".into(),"001".into()]],merges:vec![(0,0,1,0)]},
            Sheet{name:"Sheet 2".into(),rows:vec![vec!["42".into()]],merges:vec![]},
        ];
        write(path.to_str().unwrap(),&sheets).unwrap();
        let file=File::open(&path).unwrap();let mut archive=zip::ZipArchive::new(file).unwrap();
        let mut workbook=String::new();archive.by_name("xl/workbook.xml").unwrap().read_to_string(&mut workbook).unwrap();
        assert!(workbook.contains("name=\"학교\""));assert!(workbook.contains("name=\"Sheet 2\""));
        let mut sheet=String::new();archive.by_name("xl/worksheets/sheet1.xml").unwrap().read_to_string(&mut sheet).unwrap();
        assert!(sheet.contains("한글"));assert!(sheet.contains("mergeCell ref=\"A1:A2\""));assert!(sheet.contains(">001</t>"));
        let mut second=String::new();archive.by_name("xl/worksheets/sheet2.xml").unwrap().read_to_string(&mut second).unwrap();assert!(second.contains("<v>42</v>"));
        drop(archive);std::fs::remove_file(path).unwrap();
    }
}

