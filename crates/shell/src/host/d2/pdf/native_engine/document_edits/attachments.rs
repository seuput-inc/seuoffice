//! Embedded-file name-tree policy, independent of PDF object encoding.
use shell_contract::IpcValue;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Slot {
    Existing(usize),
    Added(IpcValue),
}
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Entry {
    pub name: String,
    pub slot: Slot,
}
fn field<'a>(v: &'a IpcValue, name: &str) -> Option<&'a IpcValue> {
    match v {
        IpcValue::Object(entries) => entries.iter().find(|(k, _)| k == name).map(|(_, v)| v),
        _ => None,
    }
}
fn list(v: Option<&IpcValue>) -> &[IpcValue] {
    match v {
        Some(IpcValue::Array(a)) => a,
        _ => &[],
    }
}
fn string(v: Option<&IpcValue>) -> Option<&str> {
    match v {
        Some(IpcValue::String(s)) => Some(s),
        _ => None,
    }
}
/// Existing entries carry codec-provided slot indices, preserving their original
/// name and filespec PDF objects. Reject a hierarchical /Kids tree, just as the
/// editor does, instead of silently discarding descendants.
pub(super) fn plan(
    existing: &[String],
    has_kids: bool,
    ops: &IpcValue,
) -> Result<Vec<Entry>, String> {
    if has_kids {
        return Err(
            "the embedded-files name tree uses /Kids; refusing to edit it by approximation".into(),
        );
    }
    let mut entries: Vec<Entry> = Vec::new();
    for (i, name) in existing.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        if let Some(old) = entries.iter_mut().find(|e| e.name == *name) {
            old.slot = Slot::Existing(i)
        } else {
            entries.push(Entry {
                name: name.clone(),
                slot: Slot::Existing(i),
            });
        }
    }
    for item in list(field(ops, "remove")) {
        let name = string(Some(item)).unwrap_or("");
        let Some(index) = entries.iter().position(|e| e.name == name) else {
            return Err(format!("attachment not found: {name}"));
        };
        entries.remove(index);
    }
    for add in list(field(ops, "add")) {
        let name = string(field(add, "name")).unwrap_or("");
        if entries.iter().any(|e| e.name == name) {
            return Err(format!("attachment name already used: {name}"));
        }
        entries.push(Entry {
            name: name.into(),
            slot: Slot::Added(add.clone()),
        });
    }
    // JavaScript's string comparison orders UTF-16 code units, not UTF-8 bytes.
    entries.sort_by(|a, b| a.name.encode_utf16().cmp(b.name.encode_utf16()));
    Ok(entries)
}

pub(super) fn apply(session: &mut super::session::Session, ops: &IpcValue) -> Result<(), String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::{Value, json};
    fn handle(session: &mut super::session::Session, command: Value) -> Result<u64, String> {
        session
            .exchange(command)?
            .as_u64()
            .ok_or_else(|| "invalid PDF object handle".into())
    }
    fn read(
        session: &mut super::session::Session,
        target: u64,
        key: &str,
        resolve: bool,
    ) -> Result<Option<u64>, String> {
        let v =
            session.exchange(json!({"op":"read","target":target,"key":key,"resolve":resolve}))?;
        if v.is_null() {
            Ok(None)
        } else {
            v.as_u64()
                .map(Some)
                .ok_or_else(|| "invalid PDF object handle".into())
        }
    }
    fn read_typed(
        session: &mut super::session::Session,
        target: u64,
        key: &str,
        kind: &str,
    ) -> Result<Option<u64>, String> {
        let v =
            session.exchange(json!({"op":"readMaybe","target":target,"key":key,"type":kind}))?;
        if v.is_null() {
            Ok(None)
        } else {
            v.as_u64()
                .map(Some)
                .ok_or_else(|| "invalid PDF object handle".into())
        }
    }
    fn set(
        session: &mut super::session::Session,
        target: u64,
        key: &str,
        value: Value,
    ) -> Result<(), String> {
        session.exchange(json!({"op":"set","target":target,"key":key,"value":value}))?;
        Ok(())
    }
    let catalog = handle(session, json!({"op":"catalog"}))?;
    let names = read_typed(session, catalog, "Names", "dict")?;
    let tree = if let Some(names) = names {
        read_typed(session, names, "EmbeddedFiles", "dict")?
    } else {
        None
    };
    let has_kids = if let Some(tree) = tree {
        read(session, tree, "Kids", false)?.is_some()
    } else {
        false
    };
    if has_kids {
        return Err(
            "the embedded-files name tree uses /Kids; refusing to edit it by approximation".into(),
        );
    }
    let mut originals = Vec::new();
    let mut labels = Vec::new();
    if let Some(tree) = tree
        && let Some(array) = read_typed(session, tree, "Names", "array")?
    {
        let info = session.exchange(json!({"op":"inspect","target":array}))?;
        if info.get("type").and_then(Value::as_str) == Some("array") {
            let len = info.get("size").and_then(Value::as_u64).unwrap_or(0) as usize;
            for i in (0..len.saturating_sub(1)).step_by(2) {
                let key = handle(session, json!({"op":"arrayRead","target":array,"index":i}))?;
                let spec = handle(
                    session,
                    json!({"op":"arrayRead","target":array,"index":i+1}),
                )?;
                let resolved = session.exchange(json!({"op":"resolve","target":spec}))?;
                let Some(resolved) = resolved.as_u64() else {
                    continue;
                };
                let kind = session.exchange(json!({"op":"inspect","target":resolved}))?;
                if kind.get("type").and_then(Value::as_str) != Some("dict") {
                    continue;
                }
                let name = session.exchange(json!({"op":"inspect","target":key}))?;
                let text = name
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if !text.is_empty() {
                    labels.push(text);
                    originals.push((key, spec));
                }
            }
        }
    }
    let entries = plan(&labels, has_kids, ops)?;
    let mut flat = Vec::new();
    for entry in entries {
        match entry.slot {
            Slot::Existing(index) => {
                let (name, spec) = originals[index];
                flat.push(json!({"ref":name}));
                flat.push(json!({"ref":spec}));
            }
            Slot::Added(add) => {
                let data = string(field(&add, "bytesBase64")).ok_or("invalid attachment bytes")?;
                let len = STANDARD
                    .decode(data)
                    .map_err(|e| format!("invalid attachment bytes: {e}"))?
                    .len();
                let stream = handle(
                    session,
                    json!({"op":"stream","register":true,"bytesBase64":data,"dict":{"Type":{"name":"EmbeddedFile"},"Subtype":{"name":"application/octet-stream"},"Params":{"dict":{"Size":len}},"Length":len}}),
                )?;
                let mut spec = json!({"Type":{"name":"Filespec"},"F":{"text":entry.name},"UF":{"text":entry.name},"EF":{"dict":{"F":{"ref":stream},"UF":{"ref":stream}}}});
                if let Some(desc) = string(field(&add, "description")).filter(|s| !s.is_empty()) {
                    spec["Desc"] = json!({"text":desc});
                }
                let spec = handle(
                    session,
                    json!({"op":"make","register":true,"value":{"dict":spec}}),
                )?;
                flat.push(json!({"text":entry.name}));
                flat.push(json!({"ref":spec}));
            }
        }
    }
    if let Some(tree) = tree {
        set(session, tree, "Names", json!({"array":flat}))?;
        session.exchange(json!({"op":"delete","target":tree,"key":"Limits"}))?;
    } else {
        let created = handle(
            session,
            json!({"op":"make","register":true,"value":{"dict":{"Names":{"array":flat}}}}),
        )?;
        if let Some(names) = names {
            set(session, names, "EmbeddedFiles", json!({"ref":created}))?;
        } else {
            let names = handle(
                session,
                json!({"op":"make","register":true,"value":{"dict":{"EmbeddedFiles":{"ref":created}}}}),
            )?;
            set(session, catalog, "Names", json!({"ref":names}))?;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remove_then_replace_and_reject_duplicates() {
        let add = IpcValue::Object(vec![("name".into(), IpcValue::String("a".into()))]);
        let ops = IpcValue::Object(vec![
            (
                "remove".into(),
                IpcValue::Array(vec![IpcValue::String("a".into())]),
            ),
            ("add".into(), IpcValue::Array(vec![add.clone()])),
        ]);
        let slots = plan(&["a".into()], false, &ops).unwrap();
        assert_eq!(slots[0].slot, Slot::Added(add));
        assert!(plan(&["a".into()], true, &ops).is_err());
    }
}
