//! Shared workbook reader primitives from xlsx-gateway's XML helpers.

use jscompat::TryJsRegex;
use shell_ipc_d1::sheets::engine::{EngineResult, engine_error};

pub(super) fn matches(pattern: &str, flags: &str, text: &str) -> EngineResult<Vec<String>> {
    let regex = TryJsRegex::new(pattern, flags).map_err(|error| engine_error(error.to_string()))?;
    regex
        .try_find_iter(text)
        .map(|found| {
            found
                .map(|value| value.as_str().to_owned())
                .map_err(|error| engine_error(error.to_string()))
        })
        .collect()
}

pub(super) fn first(pattern: &str, flags: &str, text: &str) -> EngineResult<Option<String>> {
    let regex = TryJsRegex::new(pattern, flags).map_err(|error| engine_error(error.to_string()))?;
    regex
        .try_exec(text)
        .map(|found| found.map(|value| value.as_str().to_owned()))
        .map_err(|error| engine_error(error.to_string()))
}

pub(super) fn attribute(text: &str, name: &str) -> EngineResult<Option<String>> {
    let pattern = format!(
        r#"(?:^|\s){}\s*=\s*(?:\"([^\"]*)\"|'([^']*)')"#,
        jscompat::regex_escape(name)
    );
    let regex = TryJsRegex::new(&pattern, "").map_err(|error| engine_error(error.to_string()))?;
    Ok(regex
        .try_exec(text)
        .map_err(|error| engine_error(error.to_string()))?
        .map(|found| {
            found
                .get(1)
                .or_else(|| found.get(2))
                .unwrap_or_default()
                .to_owned()
        }))
}

pub(super) fn decoded_attribute(text: &str, name: &str) -> EngineResult<Option<String>> {
    Ok(attribute(text, name)?.map(|value| file_parse::xlsx::workbook_decode_xml_entities(&value)))
}

#[derive(Clone, Debug)]
pub(super) struct Relationship {
    pub id: Option<String>,
    pub kind: String,
    pub target: String,
    pub external: bool,
}

pub(super) fn relationships(xml: &str) -> EngineResult<Vec<Relationship>> {
    let mut result = Vec::new();
    for element in matches(r"<Relationship\b[^>]*?\/>", "g", xml)? {
        let Some(kind) = decoded_attribute(&element, "Type")? else {
            continue;
        };
        let Some(target) = decoded_attribute(&element, "Target")? else {
            continue;
        };
        result.push(Relationship {
            id: attribute(&element, "Id")?,
            kind,
            target,
            external: attribute(&element, "TargetMode")?.as_deref() == Some("External"),
        });
    }
    Ok(result)
}

pub(super) fn rels_path(part: &str) -> String {
    let (directory, name) = part.rsplit_once('/').unwrap_or(("", part));
    format!("{directory}/_rels/{name}.rels")
}
