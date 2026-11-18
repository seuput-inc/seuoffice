//! Synthetic dependency-boundary regressions, not an HTML parser oracle.
//! Hosted execution only. No global host registration or authority override.
#![cfg(feature = "book")]

use std::cell::RefCell;
use epub_engine::{
    open::{open_epub_with_host, EpubOpenResult, EPUB_ZIP_LIMITS},
    text::extract_chapter_text_with_host,
    utf16::{lossy, units},
    xml_positions::parse_xml_with_host,
    xml_positions_types::{XmlNode, XmlParseResult},
};
use ooxml_pkg::Package;
use serde_json::{json, Value};

fn repaired_heading(title: &str) -> Value {
    json!({"elements":[
        {"name":"html","local":"html","uri":null,"attributes":[],"children":[{"element":1}]},
        {"name":"body","local":"body","uri":null,"attributes":[],"children":[{"element":2}]},
        {"name":"h1","local":"h1","uri":null,"attributes":[],"children":[title]}
    ]})
}

fn archive(chapter: &str, navigation: bool) -> Vec<u8> {
    let mut package = Package::default();
    package.set_file("mimetype", b"application/epub+zip".to_vec());
    package.set_file("META-INF/container.xml", br#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0"><rootfiles><rootfile full-path="book.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#.to_vec());
    let nav_items = if navigation { r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>"# } else { "" };
    let opf = format!(r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="uid">request-fixture</dc:identifier><dc:title>Request fixture</dc:title><dc:language>en</dc:language></metadata><manifest><item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/>{nav_items}</manifest><spine toc="ncx"><itemref idref="chapter"/></spine></package>"#);
    package.set_file("book.opf", opf.into_bytes());
    package.set_file("chapter.xhtml", chapter.as_bytes().to_vec());
    if navigation {
        package.set_file("nav.xhtml", b"<html><nav><ol><li".to_vec());
        package.set_file("toc.ncx", b"<ncx><navMap".to_vec());
    }
    ooxml_pkg::write::generate(&package, 0.0).unwrap()
}

#[test]
fn navigation_ncx_spine_and_chapter_share_the_explicit_request_parser() {
    let chapter = "<html><body><h1>Broken";
    let bytes = archive(chapter,true);
    let calls = RefCell::new(Vec::new());
    let host = |name: &str, input: &Value| {
        calls.borrow_mut().push((name.to_owned(),input.clone()));
        match name {
            "epub.parseHtml" if input["text"] == chapter => Ok(repaired_heading("Request 😀")),
            "epub.parseHtml" => Ok(json!({"elements":[{"name":"html","local":"html","uri":null,"attributes":[],"children":[]}]})),
            "epub.chapterTextRoots" => Ok(json!({"elements":[{"name":"body","children":[{"element":1}]},{"name":"h1","children":["Request 😀"]}],"roots":[{"element":0}]})),
            _ => panic!("unexpected callback {name}"),
        }
    };
    let EpubOpenResult::Opened(book) = open_epub_with_host(&bytes,&EPUB_ZIP_LIMITS,&host) else { panic!("synthetic book must open"); };
    assert_eq!(book.info.toc.source,"spine");
    assert_eq!(book.info.toc.entries[0].title,"Request 😀");
    let source = book.read_text("chapter.xhtml").unwrap().unwrap();
    assert_eq!(extract_chapter_text_with_host(&source.text,&host).unwrap(),"# Request 😀");
    let calls = calls.into_inner();
    assert_eq!(calls.iter().map(|(name,_)| name.as_str()).collect::<Vec<_>>(),
        ["epub.parseHtml","epub.parseHtml","epub.parseHtml","epub.chapterTextRoots"]);
    assert_eq!(calls[0].1["text"],"<html><nav><ol><li");
    assert_eq!(calls[1].1["text"],"<ncx><navMap");
    assert_eq!(calls[2].1["text"],chapter);
    assert_eq!(calls[3].1["text"],chapter);
}

#[test]
fn doctyped_entities_are_cached_only_within_the_injected_parse() {
    let source = "<!DOCTYPE html><html><body><p>😀&requestentity;</p></body></html>";
    for answer in ["first", "second"] {
        let calls = RefCell::new(Vec::new());
        let host = |name: &str, input: &Value| {
            calls.borrow_mut().push(name.to_owned());
            assert_eq!(name,"epub.parseFragmentText");
            assert_eq!(input["html"],"&requestentity;");
            Ok(json!(answer))
        };
        let XmlParseResult::Ok(document) = parse_xml_with_host(&units(source),&host) else { panic!("injected entity must parse"); };
        let paragraph = document.elements.iter().find(|element| element.local == "p").unwrap();
        let XmlNode::Text(text) = &paragraph.children[0] else { panic!("paragraph text missing"); };
        assert_eq!(lossy(&text.value),format!("😀{answer}"));
        let start = source.find("😀").unwrap();
        let end = source.find("</p>").unwrap();
        assert_eq!(text.start,source[..start].encode_utf16().count());
        assert_eq!(text.end,source[..end].encode_utf16().count());
        assert_eq!(calls.into_inner(),["epub.parseFragmentText"]);
    }
}

#[test]
fn a_previous_request_cannot_supply_entities_after_owner_cancellation() {
    let source = "<!DOCTYPE html><html><body><p>&requestentity;</p></body></html>";
    let allowed = |_: &str, _: &Value| Ok(json!("allowed"));
    assert_eq!(extract_chapter_text_with_host(source,&allowed).unwrap(),"allowed");
    let calls = RefCell::new(Vec::new());
    let revoked = |name: &str, _: &Value| {
        calls.borrow_mut().push(name.to_owned());
        Err(seu_host::HostError { name:"AbortError".into(), message:"request ended".into() })
    };
    let error = extract_chapter_text_with_host(source,&revoked).unwrap_err();
    assert_eq!((error.name.as_str(),error.message.as_str()),("AbortError","request ended"));
    assert_eq!(calls.into_inner(),["epub.parseFragmentText","epub.chapterTextRoots"]);
}

#[test]
fn well_formed_xml_and_zip_refusal_do_not_invoke_an_html_parser() {
    let forbidden = |name: &str, _: &Value| -> Result<Value,seu_host::HostError> { panic!("unexpected parser callback {name}"); };
    assert_eq!(extract_chapter_text_with_host("<html><body><p>safe 😀</p></body></html>",&forbidden).unwrap(),"safe 😀");
    let bytes = archive("<html><body><h1>XML title</h1></body></html>",false);
    let EpubOpenResult::Opened(book) = open_epub_with_host(&bytes,&EPUB_ZIP_LIMITS,&forbidden) else { panic!("XML book must open"); };
    assert_eq!(book.info.toc.entries[0].title,"XML title");
    assert!(matches!(open_epub_with_host(b"not a zip",&EPUB_ZIP_LIMITS,&forbidden),EpubOpenResult::Refused { .. }));
}

#[test]
fn malformed_repaired_roots_report_the_original_protocol_error() {
    let host = |name: &str, _: &Value| { assert_eq!(name,"epub.chapterTextRoots"); Ok(json!({"elements":[],"roots":[{"element":0}]})) };
    let error = extract_chapter_text_with_host("<html><body><p>",&host).unwrap_err();
    assert_eq!(error.name,"HostProtocolError");
}
