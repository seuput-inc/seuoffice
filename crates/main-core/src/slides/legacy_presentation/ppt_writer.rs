//! The original stage-one PPT writer deliberately differs from the rich PPTX
//! insertion defaults (title locks, no autofit, stacked geometry and language).
//! ZIP construction and compression use the shared native package writer.
use super::ppt::{Presentation, Sequence};
use ooxml_pkg::Package;
use std::fmt::Write;

const XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PKG_R: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const GROUP: &str = "<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>";

fn escaped(text: &str, output: &mut String) {
    for character in text.chars() {
        if !((character >= ' ' && character != '\u{fffe}' && character != '\u{ffff}')
            || matches!(character, '\t' | '\n' | '\r'))
        {
            continue;
        }
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            _ => output.push(character),
        }
    }
}
fn text_shape(
    sequence: &Sequence,
    width: i64,
    slot: usize,
    slots: usize,
    output: &mut String,
    stop: &super::Stop<'_>,
) -> Result<(), String> {
    let title = matches!(sequence.kind, 0 | 6);
    let margin = 457_200i64;
    let box_width = width - margin * 2;
    let height = 600_000i64.max((box_width as f64 / slots.max(1) as f64 / 1.6).floor() as i64);
    let top = margin + slot as i64 * height;
    let _ = write!(
        output,
        "<p:sp><p:nvSpPr><p:cNvPr id=\"{}\" name=\"\"/>",
        slot + 2
    );
    output.push_str(if title {
        "<p:cNvSpPr><a:spLocks noGrp=\"1\"/></p:cNvSpPr><p:nvPr><p:ph type=\"title\"/></p:nvPr>"
    } else {
        "<p:cNvSpPr/><p:nvPr/>"
    });
    let _ = write!(
        output,
        "</p:nvSpPr><p:spPr><a:xfrm><a:off x=\"{margin}\" y=\"{top}\"/><a:ext cx=\"{box_width}\" cy=\"{height}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/>"
    );
    for line in sequence.text.split('\n') {
        stop.check()?;
        let _ = write!(
            output,
            "<a:p><a:pPr algn=\"l\"/><a:r><a:rPr lang=\"en-US\" sz=\"{}\"{}/><a:t>",
            if title { 3200 } else { 1800 },
            if title { " b=\"1\"" } else { "" }
        );
        escaped(line, output);
        output.push_str("</a:t></a:r></a:p>");
    }
    output.push_str("</p:txBody></p:sp>");
    Ok(())
}
fn relationships(inner: &str) -> String {
    format!("{XML}<Relationships xmlns=\"{PKG_R}\">{inner}</Relationships>")
}
fn rel(id: usize, kind: &str, target: &str) -> String {
    format!("<Relationship Id=\"rId{id}\" Type=\"{R}/{kind}\" Target=\"{target}\"/>")
}
fn part_type(name: &str, kind: &str) -> String {
    format!(
        "<Override PartName=\"/{name}\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.{kind}+xml\"/>"
    )
}

pub(super) fn write(
    presentation: &Presentation,
    stop: &super::Stop<'_>,
) -> Result<Vec<u8>, String> {
    let width = jscompat::math_round(presentation.width as f64 * (914_400.0 / 576.0)) as i64;
    let height = jscompat::math_round(presentation.height as f64 * (914_400.0 / 576.0)) as i64;
    let mut slide_xmls = Vec::with_capacity(presentation.slides.len());
    for slide in &presentation.slides {
        stop.check()?;
        let mut xml = format!(
            "{XML}<p:sld xmlns:a=\"{A}\" xmlns:r=\"{R}\" xmlns:p=\"{P}\"><p:cSld><p:spTree>{GROUP}"
        );
        for (slot, sequence) in slide.iter().enumerate() {
            text_shape(sequence, width, slot, slide.len(), &mut xml, stop)?;
        }
        xml.push_str(
            "</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>",
        );
        slide_xmls.push(xml);
    }
    let mut content_types = format!(
        "{XML}<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/>{}",
        part_type("ppt/presentation.xml", "presentation.main")
    );
    for index in 1..=slide_xmls.len() {
        content_types.push_str(&part_type(&format!("ppt/slides/slide{index}.xml"), "slide"));
    }
    content_types.push_str(&part_type(
        "ppt/slideMasters/slideMaster1.xml",
        "slideMaster",
    ));
    content_types.push_str(&part_type(
        "ppt/slideLayouts/slideLayout1.xml",
        "slideLayout",
    ));
    content_types.push_str("</Types>");
    let mut ids = String::new();
    let mut slide_rels = rel(1, "slideMaster", "slideMasters/slideMaster1.xml");
    for index in 0..slide_xmls.len() {
        let _ = write!(
            ids,
            "<p:sldId id=\"{}\" r:id=\"rId{}\"/>",
            256 + index,
            index + 2
        );
        slide_rels.push_str(&rel(
            index + 2,
            "slide",
            &format!("slides/slide{}.xml", index + 1),
        ));
    }
    let mut parts = vec![
        ("[Content_Types].xml".to_owned(), content_types.into_bytes()),
        ("_rels/.rels".to_owned(), relationships(&rel(1, "officeDocument", "ppt/presentation.xml")).into_bytes()),
        ("ppt/presentation.xml".to_owned(), format!("{XML}<p:presentation xmlns:a=\"{A}\" xmlns:r=\"{R}\" xmlns:p=\"{P}\"><p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId1\"/></p:sldMasterIdLst><p:sldIdLst>{ids}</p:sldIdLst><p:sldSz cx=\"{width}\" cy=\"{height}\"/><p:notesSz cx=\"6858000\" cy=\"9144000\"/></p:presentation>").into_bytes()),
        ("ppt/_rels/presentation.xml.rels".to_owned(), relationships(&slide_rels).into_bytes()),
    ];
    for index in 1..=slide_xmls.len() {
        parts.push((
            format!("ppt/slides/_rels/slide{index}.xml.rels"),
            relationships(&rel(1, "slideLayout", "../slideLayouts/slideLayout1.xml")).into_bytes(),
        ));
    }
    parts.push(("ppt/slideMasters/slideMaster1.xml".to_owned(), format!("{XML}<p:sldMaster xmlns:a=\"{A}\" xmlns:r=\"{R}\" xmlns:p=\"{P}\"><p:cSld><p:spTree>{GROUP}</p:spTree></p:cSld><p:clrMap bg1=\"lt1\" tx1=\"dk1\" bg2=\"lt2\" tx2=\"dk2\" accent1=\"accent1\" accent2=\"accent2\" accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"/><p:sldLayoutIdLst><p:sldLayoutId id=\"2147483649\" r:id=\"rId1\"/></p:sldLayoutIdLst></p:sldMaster>").into_bytes()));
    parts.push((
        "ppt/slideMasters/_rels/slideMaster1.xml.rels".to_owned(),
        relationships(&rel(1, "slideLayout", "../slideLayouts/slideLayout1.xml")).into_bytes(),
    ));
    parts.push(("ppt/slideLayouts/slideLayout1.xml".to_owned(), format!("{XML}<p:sldLayout xmlns:a=\"{A}\" xmlns:r=\"{R}\" xmlns:p=\"{P}\" type=\"blank\"><p:cSld name=\"Blank\"><p:spTree>{GROUP}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>").into_bytes()));
    parts.push((
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels".to_owned(),
        relationships(&rel(1, "slideMaster", "../slideMasters/slideMaster1.xml")).into_bytes(),
    ));
    for (index, xml) in slide_xmls.into_iter().enumerate() {
        parts.push((
            format!("ppt/slides/slide{}.xml", index + 1),
            xml.into_bytes(),
        ));
    }
    stop.check()?;
    ooxml_pkg::write::generate(&Package::from_parts(parts), 315_532_800_000.0)
        .map_err(|error| error.to_string())
}
