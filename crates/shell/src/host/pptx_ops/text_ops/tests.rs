use super::super::run_txn;
use pptx_engine::types::SlideElement;
use shell_contract::IpcValue as V;
use shell_ipc_d1::slides::engine::TxnRequest;
use std::collections::HashMap;

fn obj(fields: Vec<(&str, V)>) -> V {
    V::Object(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn text(s: &str) -> V {
    V::String(s.into())
}
fn target(id: &str) -> V {
    obj(vec![("slide", V::Number(0.0)), ("el", text(id))])
}
fn paragraph(runs: Vec<V>) -> V {
    obj(vec![("runs", V::Array(runs))])
}
fn run(s: &str) -> V {
    obj(vec![("text", text(s))])
}
fn fixture() -> (pptx_engine::deck::OpenedPptx, String) {
    let bytes = pptx_engine::blank::create_blank_pptx(0.0).unwrap();
    let mut opened = pptx_engine::deck::open_pptx(&bytes).unwrap();
    let add = obj(vec![
        ("op", text("addElement")),
        ("target", obj(vec![("slide", V::Number(0.0))])),
        ("kind", text("textbox")),
        (
            "offset",
            obj(vec![
                ("x", V::Number(0.0)),
                ("y", V::Number(0.0)),
                ("cx", V::Number(2000000.0)),
                ("cy", V::Number(1000000.0)),
            ]),
        ),
        (
            "paragraphs",
            V::Array(vec![paragraph(vec![run("Hello 😀")])]),
        ),
    ]);
    let result = run_txn(
        &mut opened,
        &mut HashMap::new(),
        &TxnRequest::atomic(vec![add]),
    );
    assert!(result.applied, "{:?}", result.failures);
    let records = result.records.unwrap();
    let V::String(id) = &records[0].created.as_ref().unwrap()[0] else {
        panic!("created id is not a string")
    };
    let id = id.clone();
    (opened, id)
}
fn request(name: &str, id: &str, key: &str, value: V) -> V {
    obj(vec![
        ("op", text(name)),
        ("target", target(id)),
        (key, value),
    ])
}

#[test]
fn text_and_format_transactions_update_model_and_records() {
    let (mut opened, id) = fixture();
    let font = request(
        "setFont",
        &id,
        "font",
        obj(vec![
            ("fontSizePt", V::Number(24.0)),
            ("bold", V::Bool(true)),
        ]),
    );
    let edited = paragraph(vec![obj(vec![
        ("text", text("안녕 😀")),
        ("srcRun", V::Number(0.0)),
        ("fontSize", V::Number(18.0)),
    ])]);
    let edit = request("setText", &id, "paragraphs", V::Array(vec![edited]));
    let format = request(
        "setParagraphFormat",
        &id,
        "format",
        obj(vec![
            ("lineSpacingPct", V::Number(120.0)),
            ("rtl", V::Bool(false)),
        ]),
    );
    let result = run_txn(
        &mut opened,
        &mut HashMap::new(),
        &TxnRequest::atomic(vec![font, edit, format]),
    );
    assert!(result.applied, "{:?}", result.failures);
    let records = result.records.unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[1].before, V::String("Hello 😀".into()));
    let V::Object(after) = &records[1].after else {
        panic!("missing text result")
    };
    assert!(
        after
            .iter()
            .any(|(k, v)| k == "note" && matches!(v,V::String(s) if s.contains("24pt → 18pt")))
    );
    let element = opened.deck.slides[0]
        .elements
        .as_ref()
        .unwrap()
        .iter()
        .find(|e| e.id() == id)
        .unwrap();
    let SlideElement::Text(element) = element else {
        panic!("not text")
    };
    let paragraph = &element.text.as_ref().unwrap().paragraphs.as_ref().unwrap()[0];
    let run = &paragraph.runs.as_ref().unwrap()[0];
    assert_eq!(run.text.as_deref(), Some("안녕 😀"));
    assert_eq!(*run.bold, Some(true));
    assert_eq!(run.font_size.as_ref().unwrap().0, 18.0);
}

#[test]
fn invalid_range_and_invalid_links_are_rejected_before_apply() {
    let (mut opened, id) = fixture();
    let bad_size = request(
        "setFont",
        &id,
        "font",
        obj(vec![("fontSizePt", V::Number(4001.0))]),
    );
    let bad_link = request(
        "setText",
        &id,
        "paragraphs",
        V::Array(vec![paragraph(vec![obj(vec![
            ("text", text("x")),
            (
                "link",
                obj(vec![
                    ("kind", text("slide")),
                    ("slideIndex", V::Number(-1.0)),
                ]),
            ),
        ])])]),
    );
    let result = run_txn(
        &mut opened,
        &mut HashMap::new(),
        &TxnRequest::atomic(vec![bad_size, bad_link]),
    );
    assert!(!result.applied);
    let failures = result.failures.unwrap();
    assert_eq!(failures.len(), 2);
    assert!(failures[0].error.contains("font.fontSizePt"));
    assert!(failures[1].error.contains("runs[].link"));
}
