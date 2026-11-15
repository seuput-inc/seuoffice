//! Built-in slide templates from the renderer-safe pipelines catalog.
//!
//! The isolated engine service loads the original catalog, including its
//! generated cover specs and style sheets. Keeping the source of truth there
//! avoids a second, independently maintained copy of the template assets.
//! Calls are synchronous and must run off the UI thread.
use serde_json::{Value, json};

/// `listSlideTemplates()`, in the original display order and with every field.
pub fn list_slide_templates() -> Result<Vec<Value>, String> {
    let value = seu_host::call("slides.templateCatalog", &json!({}))
        .map_err(|error| format!("{}: {}", error.name, error.message))?;
    let Value::Array(templates) = value else { return Err("slide template catalog is not an array".into()) };
    // Do not silently turn a broken service response into an empty catalog.
    if templates.is_empty() {
        return Err("slide template catalog is empty".into());
    }
    let mut ids = std::collections::HashSet::new();
    for template in &templates {
        let id = template.get("id").and_then(Value::as_str)
            .filter(|id| !id.is_empty()).ok_or("slide template has no id")?;
        if !ids.insert(id) { return Err(format!("duplicate slide template id: {id}")); }
        let covers = template.get("sampleCover").and_then(Value::as_object)
            .ok_or_else(|| format!("slide template {id} has no sample covers"))?;
        if !covers.get("ko").is_some_and(Value::is_object) || !covers.get("en").is_some_and(Value::is_object) {
            return Err(format!("slide template {id} has invalid sample covers"));
        }
    }
    Ok(templates)
}

/// `findSlideTemplate`: case-insensitive id or English name, after trimming.
pub fn find_slide_template(id_or_name: &str) -> Result<Option<Value>, String> {
    let key = jscompat::trim(id_or_name).to_lowercase();
    if key.is_empty() { return Ok(None); }
    Ok(list_slide_templates()?.into_iter().find(|template| {
        ["id", "name"].into_iter().any(|field| {
            template.get(field).and_then(Value::as_str).is_some_and(|s| s.to_lowercase() == key)
        })
    }))
}

fn select_cover(templates: &[Value], id: &str, lang: &str) -> Result<Option<String>, String> {
    if lang != "ko" && lang != "en" { return Ok(None); }
    let Some(template) = templates.iter().find(|template| template.get("id").and_then(Value::as_str) == Some(id)) else {
        return Ok(None);
    };
    let cover = template.get("sampleCover").and_then(|covers| covers.get(lang))
        .filter(|cover| cover.is_object())
        .ok_or_else(|| format!("slide template {id} has no {lang} sample cover"))?;
    serde_json::to_string(cover).map(Some).map_err(|error| error.to_string())
}

/// The exact catalog cover spec as JSON, ready for the production page builder.
/// Like `renderTemplateCover`, ids are case-sensitive and only ko/en are valid.
pub fn sample_cover(id: &str, lang: &str) -> Result<Option<String>, String> {
    if lang != "ko" && lang != "en" { return Ok(None); }
    select_cover(&list_slide_templates()?, id, lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_selection_preserves_content_and_rejects_unknown_inputs() {
        let templates = vec![json!({
            "id": "test-template",
            "sampleCover": {
                "ko": { "background": "#123456", "elements": [{"text": "표지"}] },
                "en": { "background": "#abcdef", "elements": [{"text": "Cover"}] }
            }
        })];
        let ko: Value = serde_json::from_str(&select_cover(&templates, "test-template", "ko").unwrap().unwrap()).unwrap();
        let en: Value = serde_json::from_str(&select_cover(&templates, "test-template", "en").unwrap().unwrap()).unwrap();
        assert_eq!(ko, templates[0]["sampleCover"]["ko"]);
        assert_eq!(en, templates[0]["sampleCover"]["en"]);
        assert!(select_cover(&templates, "TEST-TEMPLATE", "ko").unwrap().is_none());
        assert!(select_cover(&templates, "test-template", "fr").unwrap().is_none());
        assert!(select_cover(&templates, "unknown", "en").unwrap().is_none());
        assert!(select_cover(&[json!({"id":"bad"})], "bad", "en").is_err());
    }
}
