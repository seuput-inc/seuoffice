//! Save decisions shared by disk saves and in-memory page operations.

use shell_contract::IpcValue;

fn field<'a>(value: &'a IpcValue, key: &str) -> Option<&'a IpcValue> {
    match value {
        IpcValue::Object(entries) => entries.iter().find(|(name, _)| name == key).map(|(_, value)| value),
        _ => None,
    }
}

pub(super) fn has_edits(request: &IpcValue) -> bool {
    const LISTS: &[&str] = &["markups", "annotDeletes", "drawings", "noteEdits", "formValues", "stamps", "textEdits", "textInserts", "imageEdits", "redactions", "formWidgetEdits", "formFields", "blankPages", "rotations", "deletedPages", "pageOrder"];
    LISTS.iter().any(|key| matches!(field(request, key), Some(IpcValue::Array(values)) if !values.is_empty())) ||
        field(request, "metadata").is_some_and(|value| !matches!(value, IpcValue::Undefined)) ||
        matches!(field(request, "protectionDirty"), Some(IpcValue::Bool(true)))
}

pub(super) fn unchanged_disk_copy(request: &IpcValue) -> bool {
    !has_edits(request) && match field(request, "annotations") {
        None | Some(IpcValue::Undefined | IpcValue::Null) => true,
        Some(IpcValue::String(value)) => value == "keep",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_fills_alone_do_not_rewrite_but_protection_and_annotations_do() {
        let request = IpcValue::Object(vec![("staticFormFills".into(), IpcValue::Array(vec![IpcValue::Null]))]);
        assert!(unchanged_disk_copy(&request));
        let protection = IpcValue::Object(vec![("protectionDirty".into(), IpcValue::Bool(true))]);
        assert!(!unchanged_disk_copy(&protection));
        let annotations = IpcValue::Object(vec![("annotations".into(), IpcValue::String("flatten".into()))]);
        assert!(!unchanged_disk_copy(&annotations));
    }
}
