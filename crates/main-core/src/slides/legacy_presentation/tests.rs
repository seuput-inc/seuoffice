use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct Oracle {
    cases: Vec<Case>,
    xml: Vec<XmlCase>,
    units: Vec<UnitCase>,
}
#[derive(Deserialize)]
struct Case {
    name: String,
    legacy: bool,
    input: Encoded,
    expected: Expected,
}
#[derive(Deserialize)]
struct Expected {
    error: Option<String>,
    slides: Option<usize>,
    #[serde(default)]
    warnings: Vec<String>,
    #[serde(default)]
    dropped: Vec<(String, usize)>,
    #[serde(default)]
    parts: BTreeMap<String, Encoded>,
}
#[derive(Deserialize)]
struct XmlCase {
    input: String,
    expected: Value,
}
#[derive(Deserialize)]
struct UnitCase {
    input: String,
    expected: Option<f64>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Encoded {
    Text(String),
    Chunks(Vec<String>),
}
impl Encoded {
    fn into_text(self) -> String {
        match self {
            Self::Text(value) => value,
            Self::Chunks(chunks) => chunks.concat(),
        }
    }
}
fn oracle() -> Oracle {
    serde_json::from_str(include_str!("reference-output.json")).unwrap()
}
fn stable(xml: &str) -> String {
    jscompat::js_regex!(r#"(<a16:creationId\b[^>]*\bid=")\{[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\}(")"#, "gi")
        .replace_all(xml, |found| format!("{}{{UUID}}{}", found.get(1).unwrap(), found.get(2).unwrap()))
}
fn parts(bytes: &[u8], legacy: bool) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for entry in ooxml_pkg::jszip::load(bytes, false).unwrap() {
        if entry.is_dir {
            continue;
        }
        if !legacy
            && !(entry.name == "ppt/presentation.xml"
                || entry.name.starts_with("ppt/slides/")
                || entry.name.starts_with("ppt/media/"))
        {
            continue;
        }
        let content = entry.content.unwrap();
        let value = if entry.name.ends_with(".xml") || entry.name.ends_with(".rels") {
            stable(std::str::from_utf8(&content).unwrap())
        } else {
            STANDARD.encode(content)
        };
        out.insert(entry.name, value);
    }
    out
}
#[test]
fn actual_original_codecs_define_parts_errors_warnings_and_drop_counts() {
    for case in oracle().cases {
        let input = STANDARD.decode(case.input.into_text()).unwrap();
        let result = convert_detailed(&input, case.legacy, &|| false);
        if let Some(error) = case.expected.error {
            assert_eq!(result.unwrap_err(), error, "{}", case.name);
        } else {
            let actual = result.unwrap_or_else(|error| panic!("{}: {error}", case.name));
            assert_eq!(Some(actual.slides), case.expected.slides, "{}", case.name);
            assert_eq!(actual.warnings, case.expected.warnings, "{}", case.name);
            assert_eq!(actual.dropped, case.expected.dropped, "{}", case.name);
            let expected_parts = case
                .expected
                .parts
                .into_iter()
                .map(|(name, value)| (name, value.into_text()))
                .collect::<BTreeMap<_, _>>();
            assert_eq!(
                parts(&actual.bytes, case.legacy),
                expected_parts,
                "{}",
                case.name
            );
            // Real native engine parsing proves these are document packages,
            // not IPC-shaped metadata masquerading as converted bytes.
            pptx_engine::deck::open_pptx(&actual.bytes).unwrap();
        }
    }
}
fn projected(nodes: &[xml::XNode]) -> Vec<Value> {
    nodes.iter().map(|node| match node {
        xml::XNode::Text(value) => json!({ "text": value }),
        xml::XNode::Element(element) => json!({ "name": element.name, "attrs": element.attrs.iter().cloned().collect::<BTreeMap<_, _>>(), "children": projected(&element.children) }),
    }).collect()
}
#[test]
fn original_xml_options_preserve_numeric_literals_ignore_cdata_and_bound_entities() {
    for case in oracle().xml {
        let actual = xml::parse(&case.input, &Stop(&|| false)).unwrap();
        let actual = actual
            .as_deref()
            .map(|nodes| json!(projected(nodes)))
            .unwrap_or(Value::Null);
        assert_eq!(actual, case.expected, "{}", case.input);
    }
}
#[test]
fn original_length_units_round_and_reject_unprovable_geometry() {
    for case in oracle().units {
        assert_eq!(
            odf::length(Some(&case.input)),
            case.expected,
            "{}",
            case.input
        );
    }
}
#[test]
fn cancellation_is_distinct_and_checked_inside_the_codec_not_only_at_entry() {
    assert_eq!(
        convert_presentation_cancellable(b"not opened", true, &|| true).unwrap_err(),
        "conversion cancelled"
    );
    let case = oracle()
        .cases
        .into_iter()
        .find(|case| case.name == "odp-unicode-mixed-list")
        .unwrap();
    let input = STANDARD.decode(case.input.into_text()).unwrap();
    let polls = std::cell::Cell::new(0);
    let error = convert_presentation_cancellable(&input, false, &|| {
        polls.set(polls.get() + 1);
        polls.get() > 20
    })
    .unwrap_err();
    assert_eq!(error, "conversion cancelled");
    assert!(polls.get() > 20);
}
fn record(kind: u16, version: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}
fn flat_text(text: &[u8]) -> Vec<u8> {
    let header = record(0x0f9f, 0, &4u32.to_le_bytes());
    let atom = record(0x0fa8, 0, text);
    let textbox = record(0xf00d, 0, &[header, atom].concat());
    [record(0x03e8, 15, &[]), record(0x03ee, 15, &textbox)].concat()
}
#[test]
fn ppt_text_budgets_charge_discarded_atoms_and_count_cr_vt_as_one_break() {
    let parsed = ppt::parse(&flat_text(b"a\r\x0bb\x1ec"), None, &Stop(&|| false)).unwrap();
    assert_eq!(parsed.slides[0][0].text, "a\nb c");
    let exact = vec![b'x'; 4_194_304];
    assert!(ppt::parse(&flat_text(&exact), None, &Stop(&|| false)).is_ok());
    let over = vec![b'x'; 4_194_305];
    let failure = ppt::parse(&flat_text(&over), None, &Stop(&|| false)).unwrap_err();
    assert_eq!(failure.code, "limits_exceeded");
    assert_eq!(
        failure.detail.as_deref(),
        Some("more than 4194304 characters of text")
    );
    let mut textbox = record(0x0fa8, 0, &exact);
    textbox.extend(record(0x0fa8, 0, b"replacement"));
    let bytes = [
        record(0x03e8, 15, &[]),
        record(0x03ee, 15, &record(0xf00d, 0, &textbox)),
    ]
    .concat();
    assert_eq!(
        ppt::parse(&bytes, None, &Stop(&|| false)).unwrap_err().code,
        "limits_exceeded"
    );
}
#[test]
fn ppt_paragraph_budget_accepts_the_boundary_and_refuses_the_next_paragraph() {
    assert!(ppt::parse(&flat_text(&vec![b'\r'; 262_143]), None, &Stop(&|| false)).is_ok());
    let failure =
        ppt::parse(&flat_text(&vec![b'\r'; 262_144]), None, &Stop(&|| false)).unwrap_err();
    assert_eq!(
        failure.detail.as_deref(),
        Some("more than 262144 paragraphs of text")
    );
}
#[test]
fn xml_entity_expansion_and_definition_limits_are_document_scoped() {
    let stop = Stop(&|| false);
    let entity = "x".repeat(10_000);
    let accepted = format!(
        "<!DOCTYPE a [<!ENTITY long '{entity}'>]><a>{}</a>",
        "&long;".repeat(10)
    );
    assert!(xml::parse(&accepted, &stop).unwrap().is_some());
    let refused = format!(
        "<!DOCTYPE a [<!ENTITY long '{entity}'>]><a>{}</a>",
        "&long;".repeat(11)
    );
    assert!(xml::parse(&refused, &stop).unwrap().is_none());
    let definition_too_large =
        format!("<!DOCTYPE a [<!ENTITY long '{}'>]><a/>", "x".repeat(10_001));
    assert!(xml::parse(&definition_too_large, &stop).unwrap().is_none());
    let duplicates = "<!ENTITY x 'short'>".repeat(1001);
    assert!(
        xml::parse(&format!("<!DOCTYPE a [{duplicates}]><a/>",), &stop)
            .unwrap()
            .is_none()
    );
}
fn odf_with_parts(content: &str, extra_name: &str, extra: Vec<u8>) -> Vec<u8> {
    let package = ooxml_pkg::Package::from_parts(vec![
        ("content.xml".to_owned(), content.as_bytes().to_vec()),
        (extra_name.to_owned(), extra),
    ]);
    ooxml_pkg::write::generate(&package, 315_532_800_000.0).unwrap()
}
#[test]
fn odf_part_limit_applies_to_used_parts_not_every_unused_archive_member() {
    let content =
        "<office:document><office:body><office:presentation/></office:body></office:document>";
    let unused = odf_with_parts(content, "unused.bin", vec![0; 64 * 1024 * 1024 + 1]);
    let pack = odf::Odf::load(&unused, &Stop(&|| false)).unwrap();
    assert!(pack.presentation().is_some());
    assert!(matches!(pack.image_entry("unused.bin"), Err(error) if error == "unreadable"));
    let styles = odf_with_parts(content, "styles.xml", vec![b' '; 64 * 1024 * 1024 + 1]);
    assert!(
        matches!(odf::Odf::load(&styles, &Stop(&|| false)), Err(error) if error == "unreadable")
    );
}
