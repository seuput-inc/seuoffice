//! Bookmark policy is evaluated before any PDF objects are registered.
use shell_contract::IpcValue;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct OutlineEntry {
    pub title: String,
    pub page_index: usize,
    pub top: Option<f64>,
    pub children: Vec<OutlineEntry>,
}
fn field<'a>(v: &'a IpcValue, name: &str) -> Option<&'a IpcValue> {
    match v {
        IpcValue::Object(entries) => entries.iter().find(|(key, _)| key == name).map(|(_, v)| v),
        _ => None,
    }
}
fn list(v: Option<&IpcValue>) -> &[IpcValue] {
    match v {
        Some(IpcValue::Array(a)) => a,
        _ => &[],
    }
}
fn number(v: Option<&IpcValue>) -> Option<f64> {
    match v {
        Some(IpcValue::Number(n)) => Some(*n),
        _ => None,
    }
}

/// `None` means leave the existing outline untouched. An empty vector means
/// replace it with an empty outline, which is distinct from a pruned-away tree.
pub(super) fn plan(
    value: Option<&IpcValue>,
    deleted: &[f64],
    page_count: usize,
) -> Result<Option<Vec<OutlineEntry>>, String> {
    let Some(IpcValue::Array(entries)) = value else {
        return Ok(None);
    };
    fn convert(
        entries: &[IpcValue],
        deleted: &[f64],
        count: usize,
    ) -> Result<Vec<OutlineEntry>, String> {
        let mut result = Vec::new();
        for entry in entries {
            let page = number(field(entry, "pageIndex"));
            if page.is_some_and(|p| deleted.contains(&p)) {
                continue;
            }
            let title = match field(entry, "title") {
                Some(IpcValue::String(s)) => jscompat::trim(s),
                _ => "",
            };
            if title.is_empty() {
                return Err("Outline entries need a non-empty title.".into());
            }
            let page = page
                .filter(|p| p.is_finite() && *p >= 0.0 && p.fract() == 0.0 && *p < (count as f64))
                .ok_or_else(|| {
                    let p = match field(entry, "pageIndex") {
                        Some(IpcValue::Number(n)) => n.to_string(),
                        Some(IpcValue::Null) => "null".into(),
                        Some(IpcValue::String(s)) => s.clone(),
                        _ => "undefined".into(),
                    };
                    format!("Outline entry \"{title}\" points at page {p}.")
                })?;
            let children = convert(list(field(entry, "children")), deleted, count)?;
            result.push(OutlineEntry {
                title: title.to_owned(),
                page_index: page as usize,
                top: number(field(entry, "top")),
                children,
            });
        }
        Ok(result)
    }
    // Pruning precedes validation, including validation of all children of a removed entry.
    let result = convert(entries, deleted, page_count)?;

    if !entries.is_empty() && result.is_empty() {
        Ok(None)
    } else {
        Ok(Some(result))
    }
}
/// Encode a validated tree into the already-open document without a reload.
pub(super) fn write(
    session: &mut super::session::Session,
    entries: &[OutlineEntry],
) -> Result<(), String> {
    use serde_json::{Value, json};
    fn handle(session: &mut super::session::Session, command: Value) -> Result<u64, String> {
        session
            .exchange(command)?
            .as_u64()
            .ok_or_else(|| "invalid PDF object handle".into())
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
    fn build(
        session: &mut super::session::Session,
        entries: &[OutlineEntry],
        parent: u64,
    ) -> Result<(Option<u64>, Option<u64>, usize), String> {
        let (mut first, mut previous, mut count) = (None, None, 0);
        for entry in entries {
            let page = handle(session, json!({"op":"pageRef","index":entry.page_index}))?;
            let top = entry
                .top
                .map(|v| json!(jscompat::math_round(v * 100.0) / 100.0))
                .unwrap_or(Value::Null);
            let item = handle(
                session,
                json!({"op":"make","register":true,"value":{"dict":{"Title":{"text":entry.title},"Parent":{"ref":parent},"Dest":{"array":[{"ref":page},{"name":"XYZ"},0,top,null]}}}}),
            )?;
            let dict = handle(session, json!({"op":"resolve","target":item}))?;
            if let Some(prev) = previous {
                let prev_dict = handle(session, json!({"op":"resolve","target":prev}))?;
                set(session, prev_dict, "Next", json!({"ref":item}))?;
                set(session, dict, "Prev", json!({"ref":prev}))?;
            }
            if first.is_none() {
                first = Some(item)
            }
            previous = Some(item);
            count += 1;
            if !entry.children.is_empty() {
                let (child_first, child_last, children) = build(session, &entry.children, item)?;
                if let Some(v) = child_first {
                    set(session, dict, "First", json!({"ref":v}))?;
                }
                if let Some(v) = child_last {
                    set(session, dict, "Last", json!({"ref":v}))?;
                }
                set(session, dict, "Count", json!(children))?;
                count += children;
            }
        }
        Ok((first, previous, count))
    }
    let root = handle(
        session,
        json!({"op":"make","register":true,"value":{"dict":{"Type":{"name":"Outlines"}}}}),
    )?;
    let (first, last, total) = build(session, entries, root)?;
    let root_dict = handle(session, json!({"op":"resolve","target":root}))?;
    if let Some(first) = first {
        set(session, root_dict, "First", json!({"ref":first}))?;
    }
    if let Some(last) = last {
        set(session, root_dict, "Last", json!({"ref":last}))?;
    }
    set(session, root_dict, "Count", json!(total))?;
    let catalog = handle(session, json!({"op":"catalog"}))?;
    set(session, catalog, "Outlines", json!({"ref":root}))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn entry(title: &str, page: f64, children: Vec<IpcValue>) -> IpcValue {
        IpcValue::Object(vec![
            ("title".into(), IpcValue::String(title.into())),
            ("pageIndex".into(), IpcValue::Number(page)),
            ("children".into(), IpcValue::Array(children)),
        ])
    }
    #[test]
    fn empty_tree_differs_from_fully_pruned_tree() {
        assert_eq!(
            plan(Some(&IpcValue::Array(vec![])), &[], 2).unwrap(),
            Some(vec![])
        );
        let removed = IpcValue::Array(vec![entry("gone", 1.0, vec![entry("", 999.0, vec![])])]);
        assert_eq!(plan(Some(&removed), &[1.0], 2).unwrap(), None);
    }
    #[test]
    fn validates_surviving_tree_before_writing() {
        let tree = IpcValue::Array(vec![entry(" kept ", 0.0, vec![entry("bad", 2.0, vec![])])]);
        assert_eq!(
            plan(Some(&tree), &[], 2).unwrap_err(),
            "Outline entry \"bad\" points at page 2."
        );
    }
}
