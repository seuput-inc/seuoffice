//! Mail-merge data source shaping from apps/docs/src/main/merge-data-source.ts.

use std::collections::HashMap;
use docx_engine::save::features::mail_merge::merge_data_from_rows;
use serde_json::{Value, json};

pub(super) fn from_rows(path: &str, mut rows: Vec<Vec<String>>) -> Value {
    if let Some(header) = rows.first_mut() {
        let mut seen = HashMap::<String, usize>::new();
        for (index, cell) in header.iter_mut().enumerate() {
            let trimmed = jscompat::trim(cell);
            let name = if trimmed.is_empty() { format!("Column{}", index + 1) } else { trimmed.to_owned() };
            let count = seen.entry(name.clone()).or_default();
            *count += 1;
            *cell = if *count == 1 { name } else { format!("{name}{count}") };
        }
    }
    let data = merge_data_from_rows(&rows);
    json!({
        "path": path,
        "name": jscompat::node_path::host::basename(path, None),
        "fields": data.fields,
        "records": data.records,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_and_empty_headers_are_named_before_records() {
        let rows = vec![vec![" Name ".into(), "Name".into(), "".into()], vec!["A".into(), "B".into(), "C".into()]];
        let answer = from_rows("/work/list.csv", rows);
        assert_eq!(answer["fields"], json!(["Name", "Name2", "Column3"]));
        assert_eq!(answer["records"][0], json!({"Name":"A", "Name2":"B", "Column3":"C"}));
    }
}
