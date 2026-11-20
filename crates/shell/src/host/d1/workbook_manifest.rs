//! Workbook archive integrity checks from xlsx-gateway's xlsx-package-io.ts.

use std::collections::{HashMap, HashSet};

use shell_ipc_d1::sheets::engine::{EngineResult, engine_error};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Entry {
    pub name: String,
    pub crc32: f64,
    pub compressed_size: f64,
    pub uncompressed_size: f64,
}

pub(super) fn assert_preserved(
    before: &[Entry],
    after: &[Entry],
    replaced: &HashSet<String>,
    added: &[String],
    removed: &HashSet<String>,
) -> EngineResult<()> {
    let before_by_name: HashMap<_, _> = before
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();
    let after_by_name: HashMap<_, _> = after
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();
    let added_names: HashSet<&str> = added.iter().map(String::as_str).collect();
    for entry in before {
        let saved = after_by_name.get(entry.name.as_str());
        if removed.contains(&entry.name) {
            if saved.is_some() {
                return Err(engine_error(format!(
                    "Saving should have removed {} but did not — aborted.",
                    entry.name
                )));
            }
            continue;
        }
        let Some(saved) = saved else {
            return Err(engine_error(format!(
                "Saving would drop {} — aborted.",
                entry.name
            )));
        };
        if replaced.contains(&entry.name) {
            continue;
        }
        if saved.crc32 != entry.crc32
            || saved.compressed_size != entry.compressed_size
            || saved.uncompressed_size != entry.uncompressed_size
        {
            return Err(engine_error(format!(
                "Saving would unexpectedly modify {} — aborted.",
                entry.name
            )));
        }
    }
    for name in added {
        if before_by_name.contains_key(name.as_str()) {
            return Err(engine_error(format!(
                "Saving should have created {name} but it already existed — aborted."
            )));
        }
        if !after_by_name.contains_key(name.as_str()) {
            return Err(engine_error(format!(
                "Saving should have created {name} but did not — aborted."
            )));
        }
    }
    for entry in after {
        if !before_by_name.contains_key(entry.name.as_str())
            && !added_names.contains(entry.name.as_str())
        {
            return Err(engine_error(format!(
                "Saving would unexpectedly create {} — aborted.",
                entry.name
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, crc32: f64) -> Entry {
        Entry {
            name: name.into(),
            crc32,
            compressed_size: 12.0,
            uncompressed_size: 30.0,
        }
    }

    #[test]
    fn save_rejects_changes_to_undeclared_workbook_parts() {
        let before = vec![
            entry("xl/styles.xml", 1.0),
            entry("xl/worksheets/sheet1.xml", 2.0),
        ];
        let mut after = before.clone();
        after[1].crc32 = 3.0;
        let empty = HashSet::new();
        assert!(assert_preserved(&before, &after, &empty, &[], &empty).is_err());
        let replaced = HashSet::from(["xl/worksheets/sheet1.xml".to_owned()]);
        assert!(assert_preserved(&before, &after, &replaced, &[], &empty).is_ok());
        after.pop();
        assert!(assert_preserved(&before, &after, &replaced, &[], &empty).is_err());
    }
}
