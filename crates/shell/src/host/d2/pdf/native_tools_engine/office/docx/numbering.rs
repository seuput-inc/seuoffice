//! Allocate real numbering definitions for detected PDF list sequences.
use super::super::ir::ListInfo;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct Numbering {
    by_run: HashMap<(usize, usize), String>,
    new_defs: Vec<Value>,
    restarts: Vec<Value>,
}
impl Numbering {
    pub fn id_for(&mut self, page: usize, list: &ListInfo) -> String {
        if list.kind == "bullet" {
            return "1".into();
        }
        let key = (page, list.sequence_id.unwrap_or(0));
        if let Some(id) = self.by_run.get(&key) {
            return id.clone();
        }
        let id = (100 + self.by_run.len()).to_string();
        self.by_run.insert(key, id.clone());
        let start = list.start.unwrap_or(1);
        match list.style.as_deref() {
            Some("multi") => {
                let levels: Vec<_> = (0..5).map(|i| json!({"numFmt": "decimal", "lvlText": format!("{}.", (1..=i+1).map(|n| format!("%{n}")).collect::<Vec<_>>().join(".")), "indentLeft": 720, "hanging": 720, "start": list.start_values.as_ref().and_then(|s| s.get(i)).copied().unwrap_or(1)})).collect();
                self.new_defs
                    .push(json!({"numId": id, "kind": "ordered", "levels": levels}));
            }
            Some(style @ ("paren" | "parens")) => {
                let levels: Vec<_> = (0..5).map(|i| {
                    let marker = if i == list.level { if style == "paren" { format!("%{})", i+1) } else { format!("(%{})", i+1) } } else { format!("%{}.", i+1) };
                    json!({"numFmt": "decimal", "lvlText": marker, "indentLeft": 720 * (i+1), "hanging": 360, "start": if i == list.level { start } else { 1 }})
                }).collect();
                self.new_defs
                    .push(json!({"numId": id, "kind": "ordered", "levels": levels}));
            }
            _ => {
                let overrides: Map<String, Value> = (0..5)
                    .map(|i| {
                        (
                            i.to_string(),
                            json!(if i == list.level { start } else { 1 }),
                        )
                    })
                    .collect();
                self.restarts
                    .push(json!({"numId": id, "abstractNumId": "1", "startOverrides": overrides}));
            }
        }
        id
    }
    pub fn options(&self) -> Option<Value> {
        if self.new_defs.is_empty() && self.restarts.is_empty() {
            return None;
        }
        let mut options = Map::new();
        if !self.new_defs.is_empty() {
            options.insert("newDefs".into(), json!(self.new_defs));
        }
        if !self.restarts.is_empty() {
            options.insert("restartNums".into(), json!(self.restarts));
        }
        Some(Value::Object(options))
    }
}
