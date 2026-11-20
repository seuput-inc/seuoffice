//! Prepare PDF metadata updates using the editor's keyword rules.

use serde_json::{Map, Value};
use shell_contract::IpcValue;

pub(super) fn apply(source: &[u8], metadata: &IpcValue, modified_at: f64) -> Result<Vec<u8>, String> {
    let IpcValue::Object(entries) = metadata else { return Err("invalid PDF metadata".into()) };
    let mut values = Map::new();
    for (name, value) in entries {
        if !matches!(name.as_str(), "title" | "author" | "subject" | "keywords") || matches!(value, IpcValue::Undefined) { continue; }
        let IpcValue::String(value) = value else { return Err(format!("invalid PDF metadata {name}")) };
        if name == "keywords" {
            let words = value.split([',', '，', ';', '；']).map(jscompat::trim).filter(|word| !word.is_empty()).map(|word| Value::String(word.to_owned())).collect();
            values.insert(name.clone(), Value::Array(words));
        } else {
            values.insert(name.clone(), Value::String(value.clone()));
        }
    }
    let mut input = Map::new();
    input.insert("metadata".into(), Value::Object(values));
    input.insert("modifiedAt".into(), Value::from(modified_at));
    let value = super::super::page_ops::call("pdf.applyMetadata", source, input)?;
    super::super::page_ops::bytes(&value)
}
