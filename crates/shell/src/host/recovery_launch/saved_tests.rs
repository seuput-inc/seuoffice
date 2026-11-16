use super::*;
use serde_json::json;
use shell_contract::ViewId;
use shell_ipc_w::tabs::SavedSessionUpdate;

fn payload() -> SessionPayload {
    SessionPayload {
        strip: json!({"tabs":[{"id":"a","kind":"docs","title":"Untitled"},{"id":"b","kind":"html","title":"Other"}],"active":"b","split":{"a":"a","b":"b"}}),
        detached: Vec::new(),
        views: vec![("a".into(), ViewId(7)), ("b".into(), ViewId(8))],
        copies: [
            (
                "a".into(),
                Some(jscompat::json_value_from_serde(
                    &json!({"untitledId":"copy-a","ext":"docx"}),
                )),
            ),
            (
                "b".into(),
                Some(jscompat::json_value_from_serde(
                    &json!({"untitledId":"copy-b","ext":"html"}),
                )),
            ),
        ]
        .into(),
    }
}
fn saved(id: &str, view: u32, path: &str) -> SavedSessionUpdate {
    SavedSessionUpdate {
        id: id.into(),
        view: Some(ViewId(view)),
        path: Some(path.into()),
        title: "saved".into(),
    }
}

#[test]
fn a_saved_delta_keeps_intervening_new_tabs_layout_and_other_copy_identity() {
    let mut current = payload();
    current.strip["tabs"]
        .as_array_mut()
        .unwrap()
        .insert(0, json!({"id":"new","kind":"mindmap","title":"New"}));
    current.views.push(("new".into(), ViewId(9)));
    current.copies.insert(
        "new".into(),
        Some(jscompat::json_value_from_serde(
            &json!({"untitledId":"new-copy","ext":"xmind"}),
        )),
    );
    let other = current.copies["b"].clone();
    let new = current.copies["new"].clone();
    current.apply(&[saved("a", 7, "folder/final.docx")]);
    assert_eq!(current.strip["tabs"][0]["id"], "new");
    assert_eq!(current.strip["active"], "b");
    assert_eq!(current.strip["split"], json!({"a":"a","b":"b"}));
    assert_eq!(current.strip["tabs"][1]["filePath"], "folder/final.docx");
    assert_eq!(current.copies["a"], None);
    assert_eq!(current.copies["b"], other);
    assert_eq!(current.copies["new"], new);
}

#[test]
fn two_accepted_save_deltas_merge_instead_of_overwriting_each_others_rows() {
    let mut current = payload();
    current.apply(&[saved("a", 7, "a.docx")]);
    current.apply(&[saved("b", 8, "b.html")]);
    assert_eq!(current.strip["tabs"][0]["filePath"], "a.docx");
    assert_eq!(current.strip["tabs"][1]["filePath"], "b.html");
}

#[test]
fn a_closed_or_replaced_row_is_not_resurrected_by_a_completed_old_save() {
    let mut current = payload();
    current.strip["tabs"].as_array_mut().unwrap().remove(0);
    current.views.retain(|(id, _)| id != "a");
    current.apply(&[saved("a", 7, "old.docx")]);
    assert_eq!(current.strip["tabs"].as_array().unwrap().len(), 1);
    current.strip["tabs"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"a","kind":"docs","title":"Replacement"}));
    current.views.push(("a".into(), ViewId(77)));
    current.apply(&[saved("a", 7, "old.docx")]);
    assert_eq!(current.strip["tabs"][1]["title"], "Replacement");
    assert!(current.strip["tabs"][1].get("filePath").is_none());
}

#[test]
fn provisional_title_is_not_a_saved_file_and_keeps_the_untitled_copy() {
    let mut current = payload();
    let copy = current.copies["a"].clone();
    current.apply(&[SavedSessionUpdate {
        id: "a".into(),
        view: Some(ViewId(7)),
        path: None,
        title: "Draft title".into(),
    }]);
    assert_eq!(current.strip["tabs"][0]["title"], "Draft title");
    assert_eq!(current.copies["a"], copy);
}

#[test]
fn detached_save_changes_only_its_captured_row_and_title() {
    let mut current = payload();
    let row = current.strip["tabs"].as_array_mut().unwrap().remove(0);
    current.detached.push(row);
    current.apply(&[saved("a", 7, "final.docx")]);
    assert_eq!(current.detached[0]["filePath"], "final.docx");
    assert_eq!(current.detached[0]["title"], "saved");
    assert_eq!(current.strip["tabs"][0]["id"], "b");
}
