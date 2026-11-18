use std::io::Read;

use base64::{alphabet::STANDARD, Engine as _, engine::{DecodePaddingMode, general_purpose::{GeneralPurpose, GeneralPurposeConfig}}};
use flate2::read::DeflateDecoder;
use saxes::{Handler, HandlerResult, Options, Parser, Tag};
use shell_ipc_d1::diagram::{DiagramFile, DiagramFormat, OpenFailure};

fn failure(reason: &str, message: &str) -> OpenFailure {
    OpenFailure { reason: reason.into(), message: message.into() }
}

fn base64_bytes(text: &str) -> Option<Vec<u8>> {
    let clean: String = text.chars().filter(|c| !jscompat::is_js_space(*c)).collect();
    let clean = clean.trim_end_matches('=');
    let engine = GeneralPurpose::new(&STANDARD, GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent).with_decode_allow_trailing_bits(true));
    engine.decode(clean).ok()
}

fn uri_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() || !bytes[index + 1].is_ascii_hexdigit() || !bytes[index + 2].is_ascii_hexdigit() { return None; }
            index += 3;
        } else { index += 1; }
    }
    percent_encoding::percent_decode(bytes).decode_utf8().ok().map(|text| text.into_owned())
}

fn embedded(content: &str) -> Option<String> {
    let text = content.trim_matches(jscompat::is_js_space);
    if text.starts_with('<') { return Some(text.to_owned()); }
    if text.starts_with('%') { return uri_decode(text); }
    let bytes = base64_bytes(text)?;
    let latin: String = bytes.iter().map(|byte| char::from(*byte)).collect();
    if latin.starts_with('%') { return uri_decode(&latin); }
    if latin.trim_start_matches(jscompat::is_js_space).starts_with('<') { return String::from_utf8(bytes).ok(); }
    None
}

fn inflate(data: &[u8], zlib: bool) -> Result<Vec<u8>, ()> {
    let data = if zlib {
        if data.len() < 6 || data[0] & 0x0f != 8 { return Err(()); }
        &data[2..data.len() - 4]
    } else { data };
    let mut output = Vec::new();
    DeflateDecoder::new(data).read_to_end(&mut output).map_err(|_| ())?;
    Ok(output)
}

fn png_text(bytes: &[u8]) -> Result<Option<String>, OpenFailure> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") { return Err(failure("corrupt_image", "the file is not a readable PNG")); }
    let mut pos = 8usize;
    while bytes.len().saturating_sub(pos) >= 8 {
        let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().map_err(|_| failure("corrupt_image", "the file is not a readable PNG"))?) as usize;
        let end = pos.checked_add(12).and_then(|p| p.checked_add(len)).filter(|end| *end <= bytes.len())
            .ok_or_else(|| failure("corrupt_image", "the file is not a readable PNG"))?;
        let kind = &bytes[pos + 4..pos + 8];
        let data = &bytes[pos + 8..pos + 8 + len];
        if kind == b"tEXt" || kind == b"zTXt" || kind == b"iTXt" {
            if let Some(nul) = data.iter().position(|byte| *byte == 0) {
                if &data[..nul] == b"mxfile" || &data[..nul] == b"mxGraphModel" {
                    let decode_error = || failure("decompress_failed", "the diagram inside the PNG cannot be decoded");
                    return if kind == b"tEXt" {
                        let value = &data[nul + 1..];
                        Ok(Some(std::str::from_utf8(value).map(str::to_owned)
                            .unwrap_or_else(|_| value.iter().map(|byte| char::from(*byte)).collect())))
                    } else if kind == b"zTXt" {
                        let raw = inflate(data.get(nul + 2..).ok_or_else(decode_error)?, true).map_err(|_| decode_error())?;
                        Ok(Some(raw.iter().map(|byte| char::from(*byte)).collect()))
                    } else {
                        let flag = *data.get(nul + 1).ok_or_else(|| failure("corrupt_image", "the file is not a readable PNG"))?;
                        let start = nul + 3;
                        let language = data.get(start..).and_then(|tail| tail.iter().position(|byte| *byte == 0)).map(|n| start + n)
                            .ok_or_else(|| failure("corrupt_image", "the file is not a readable PNG"))?;
                        let translated = data.get(language + 1..).and_then(|tail| tail.iter().position(|byte| *byte == 0)).map(|n| language + 1 + n)
                            .ok_or_else(|| failure("corrupt_image", "the file is not a readable PNG"))?;
                        let value = &data[translated + 1..];
                        let raw = if flag == 1 { inflate(value, true).map_err(|_| decode_error())? } else { value.to_vec() };
                        let raw = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw);
                        Ok(Some(std::str::from_utf8(raw).map_err(|_| decode_error())?.to_owned()))
                    };
                }
            }
        }
        if kind == b"IEND" { break; }
        pos = end;
    }
    Ok(None)
}

#[derive(Default)]
struct Page { children: usize, first: Option<String>, text: String }

struct XmlInfo { root: String, content: Option<String>, pages: Vec<Page> }

#[derive(Default)]
struct XmlBuilder {
    depth: usize,
    root: Option<String>,
    content: Option<String>,
    pages: Vec<Page>,
    current: Option<usize>,
    error: Option<String>,
}

impl Handler for XmlBuilder {
    fn wants_text(&self) -> bool { true }
    fn wants_error(&self) -> bool { true }
    fn error(&mut self, _: &mut Parser, error: &saxes::Error) -> HandlerResult {
        if self.error.is_none() { self.error = Some(error.message_lossy()); }
        Ok(())
    }
    fn opentag(&mut self, _: &mut Parser, tag: &Tag) -> HandlerResult {
        let name = String::from_utf16_lossy(&tag.name);
        if self.depth == 0 {
            if self.root.is_none() { self.root = Some(name.clone()); }
            if name == "svg" {
                self.content = tag.attributes.iter().find(|attribute| attribute.name == saxes::units("content"))
                    .map(|attribute| String::from_utf16_lossy(&attribute.value));
            }
        } else if self.depth == 1 && self.root.as_deref() == Some("mxfile") && name == "diagram" {
            self.pages.push(Page::default());
            self.current = Some(self.pages.len() - 1);
        } else if self.depth == 2 {
            if let Some(page) = self.current.and_then(|index| self.pages.get_mut(index)) {
                page.children += 1;
                if page.first.is_none() { page.first = Some(name); }
            }
        }
        self.depth += 1;
        Ok(())
    }
    fn closetag(&mut self, _: &mut Parser, _: &Tag) -> HandlerResult {
        self.depth = self.depth.saturating_sub(1);
        if self.depth == 1 { self.current = None; }
        Ok(())
    }
    fn text(&mut self, _: &mut Parser, text: &[u16]) -> HandlerResult {
        if self.depth == 2 {
            if let Some(page) = self.current.and_then(|index| self.pages.get_mut(index)) {
                page.text.push_str(&String::from_utf16_lossy(text));
            }
        }
        Ok(())
    }
}

fn xml_info(xml: &str) -> Result<XmlInfo, OpenFailure> {
    let mut parser = Parser::new(Options::default()).map_err(|error| failure("not_xml", &error.message_lossy()))?;
    let mut builder = XmlBuilder::default();
    if let Err(error) = parser.write_str(&mut builder, xml).and_then(|()| parser.close(&mut builder)) {
        if builder.error.is_none() { builder.error = Some(error.message_lossy()); }
    }
    if let Some(error) = builder.error { return Err(failure("not_xml", &error)); }
    Ok(XmlInfo { root: builder.root.ok_or_else(|| failure("not_xml", "no root element"))?, content: builder.content, pages: builder.pages })
}

pub(super) fn open(name: &str, bytes: &[u8]) -> Result<DiagramFile, OpenFailure> {
    let name = name.to_lowercase();
    let format = if name.ends_with(".drawio.svg") { DiagramFormat::DrawioSvg }
        else if name.ends_with(".drawio.png") { DiagramFormat::DrawioPng }
        else if name.ends_with(".drawio") { DiagramFormat::Drawio }
        else if name.ends_with(".dio") { DiagramFormat::Dio }
        else if name.ends_with(".vsdx") { return Err(failure("unsupported", "Visio files are imported by the app, not read as diagrams")); }
        else { return Err(failure("unsupported", "only .drawio, .dio, .drawio.svg and .drawio.png files are diagrams")); };
    let xml = match format {
        DiagramFormat::Drawio | DiagramFormat::Dio => std::str::from_utf8(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes))
            .map_err(|_| failure("not_xml", "the file is not UTF-8 text"))?.to_owned(),
        DiagramFormat::DrawioSvg => {
            let text = std::str::from_utf8(bytes).map_err(|_| failure("not_xml", "the SVG is not UTF-8 text"))?;
            let svg = xml_info(text.strip_prefix('\u{feff}').unwrap_or(text))
                .map_err(|error| failure("not_xml", &format!("the SVG is not XML: {}", error.message)))?;
            let content = if svg.root == "svg" { svg.content } else { None }
                .filter(|text| !text.trim_matches(jscompat::is_js_space).is_empty())
                .ok_or_else(|| failure("no_original", "the SVG has no draw.io diagram inside"))?;
            embedded(&content).ok_or_else(|| failure("decompress_failed", "the diagram inside the SVG cannot be decoded"))?
        }
        DiagramFormat::DrawioPng => {
            let text = png_text(bytes)?.ok_or_else(|| failure("no_original", "the PNG has no draw.io diagram inside"))?;
            embedded(&text).ok_or_else(|| failure("decompress_failed", "the diagram inside the PNG cannot be decoded"))?
        }
    };
    let parsed = xml_info(&xml).map_err(|error| failure("not_xml", &format!("the file is not XML: {}", error.message)))?;
    let mut compressed = false;
    if parsed.root == "mxfile" {
        if parsed.pages.is_empty() { return Err(failure("no_diagram", "the diagram has no pages")); }
        for page in parsed.pages {
            if page.children > 0 {
                if page.children != 1 || page.first.as_deref() != Some("mxGraphModel") {
                    let name = page.first.as_deref().unwrap_or("");
                    return Err(failure("no_diagram", &format!("a page holds <{name}>, not <mxGraphModel>")));
                }
            } else {
                let text = page.text.trim_matches(jscompat::is_js_space);
                if text.is_empty() { continue; }
                let raw = base64_bytes(text).filter(|bytes| !bytes.is_empty())
                    .ok_or_else(|| failure("decompress_failed", "a compressed page cannot be decoded"))?;
                let raw = inflate(&raw, false).map_err(|_| failure("decompress_failed", "a compressed page cannot be decoded"))?;
                let latin: String = raw.iter().map(|byte| char::from(*byte)).collect();
                let model = uri_decode(&latin).ok_or_else(|| failure("decompress_failed", "a compressed page cannot be decoded"))?;
                let page = xml_info(&model).map_err(|error| failure("not_xml", &format!("the page is not XML: {}", error.message)))?;
                if page.root != "mxGraphModel" {
                    return Err(failure("no_diagram", &format!("the page holds <{}>, not <mxGraphModel>", page.root)));
                }
                compressed = true;
            }
        }
    } else if parsed.root != "mxGraphModel" {
        return Err(failure("no_diagram", &format!("the XML is <{}>, not a draw.io diagram", parsed.root)));
    }
    Ok(DiagramFile { xml, format, root: parsed.root, any_page_compressed: compressed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use flate2::{Compression, write::{DeflateEncoder, ZlibEncoder}};

    fn deflate(bytes: &[u8], zlib: bool) -> Vec<u8> {
        if zlib {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(bytes).expect("zlib input");
            encoder.finish().expect("zlib output")
        } else {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(bytes).expect("deflate input");
            encoder.finish().expect("deflate output")
        }
    }

    fn png_chunk(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let mut result = (content.len() as u32).to_be_bytes().to_vec();
        result.extend_from_slice(kind);
        result.extend_from_slice(content);
        result.extend_from_slice(&[0; 4]);
        result
    }

    #[test]
    fn opens_embedded_svg_and_png_originals() {
        let xml = "<mxfile><diagram><mxGraphModel/></diagram></mxfile>";
        let encoded = base64::engine::general_purpose::STANDARD.encode(xml.as_bytes());
        let svg = format!("<svg content=\"{encoded}\"/>");
        assert_eq!(open("picture.drawio.svg", svg.as_bytes()).expect("SVG").xml, xml);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut text = b"mxfile\0\0".to_vec();
        text.extend(deflate(encoded.as_bytes(), true));
        png.extend(png_chunk(b"zTXt", &text));
        assert_eq!(open("picture.drawio.png", &png).expect("PNG").xml, xml);
    }

    #[test]
    fn opens_png_text_variants_and_rejects_missing_originals() {
        let xml = "<mxGraphModel/>";
        for (kind, content) in [
            (b"tEXt", [b"mxGraphModel\0".as_slice(), xml.as_bytes()].concat()),
            (b"iTXt", [b"mxfile\0\x00\x00\0\0".as_slice(), xml.as_bytes()].concat()),
            (b"iTXt", [b"mxfile\0\x01\x00\0\0".as_slice(), deflate(xml.as_bytes(), true).as_slice()].concat()),
        ] {
            let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
            png.extend(png_chunk(kind, &content));
            assert_eq!(open("a.drawio.png", &png).expect("PNG original").xml, xml);
        }
        assert_eq!(open("a.drawio.png", b"\x89PNG\r\n\x1a\n").expect_err("missing original").reason, "no_original");
        assert_eq!(open("a.drawio.svg", b"<svg/>").expect_err("missing original").reason, "no_original");
    }

    #[test]
    fn validates_compressed_pages_and_preserves_xml() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(deflate(b"%3CmxGraphModel%2F%3E", false));
        let xml = format!("<mxfile><diagram>{encoded}</diagram></mxfile>");
        let file = open("sample.dio", xml.as_bytes()).expect("diagram");
        assert!(file.any_page_compressed);
        assert_eq!(file.xml, xml);
        assert_eq!(open("sample.drawio", b"<mxfile/>").expect_err("missing page").reason, "no_diagram");
        assert_eq!(open("sample.drawio", b"<mxfile><diagram>!!!</diagram></mxfile>").expect_err("bad compressed page").reason, "decompress_failed");
        assert_eq!(open("sample.drawio", b"<mxGraphModel>\0</mxGraphModel>").expect_err("illegal XML character").reason, "not_xml");
        assert_eq!(open("sample.drawio", b"<![CDATA[outside]]><mxGraphModel/>").expect_err("CDATA outside root").reason, "not_xml");
        assert_eq!(open("sample.drawio", b"<1invalid/>").expect_err("invalid element name").reason, "not_xml");
        assert_eq!(open("sample.drawio", b"<mxGraphModel>abc]]>def</mxGraphModel>").expect_err("invalid text").reason, "not_xml");
    }

    #[test]
    fn reports_original_saxes_diagnostics_at_each_open_boundary() {
        for (xml, diagnostic) in [
            ("", "1:0: document must contain a root element."),
            ("<a/><b/>", "1:7: documents may contain only one root."),
            ("<a>&😀;</a>", "1:6: undefined entity."),
            ("😀<a/>😀", "1:2: text data outside of root node."),
        ] {
            let error = open("a.drawio", xml.as_bytes()).expect_err("malformed file");
            assert_eq!(error.reason, "not_xml");
            assert_eq!(error.message, format!("the file is not XML: {diagnostic}"));

            let error = open("a.drawio.svg", xml.as_bytes()).expect_err("malformed SVG");
            assert_eq!(error.reason, "not_xml");
            assert_eq!(error.message, format!("the SVG is not XML: {diagnostic}"));

            let escaped: String = xml.as_bytes().iter().map(|byte| format!("%{byte:02X}")).collect();
            let encoded = base64::engine::general_purpose::STANDARD.encode(deflate(escaped.as_bytes(), false));
            let container = format!("<mxfile><diagram>{encoded}</diagram></mxfile>");
            let error = open("a.drawio", container.as_bytes()).expect_err("malformed page");
            assert_eq!(error.reason, "not_xml");
            assert_eq!(error.message, format!("the page is not XML: {diagnostic}"));
        }
    }

    #[test]
    fn preserves_unicode_and_original_file_validation_errors() {
        let xml = "<mxfile><diagram name=\"한글 😀\"><mxGraphModel/></diagram></mxfile>";
        let file = open("unicode.drawio", xml.as_bytes()).expect("Unicode diagram");
        assert_eq!(file.xml, xml);
        assert_eq!(file.root, "mxfile");
        assert!(!file.any_page_compressed);
        let error = open("invalid.drawio", b"\xff").expect_err("invalid UTF-8");
        assert_eq!(error.reason, "not_xml");
        assert_eq!(error.message, "the file is not UTF-8 text");
        let error = open("invalid.drawio.svg", b"\xff").expect_err("invalid SVG UTF-8");
        assert_eq!(error.reason, "not_xml");
        assert_eq!(error.message, "the SVG is not UTF-8 text");
    }

    #[test]
    fn opens_original_format_fixtures_without_rewriting_xml() {
        let drawio = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../packages/drawio-model/tests/fixtures/out.drawio"));
        let svg = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../packages/drawio-model/tests/fixtures/out.drawio.svg"));
        let png = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../packages/drawio-model/tests/fixtures/out.drawio.png"));
        let plain = open("out.drawio", drawio).expect("drawio fixture");
        let embedded = open("out.drawio.png", png).expect("PNG fixture");
        let compressed = open("out.drawio.svg", svg).expect("SVG fixture");
        assert_eq!(plain.xml, embedded.xml);
        assert!(!plain.any_page_compressed);
        assert!(compressed.any_page_compressed);
    }
}
