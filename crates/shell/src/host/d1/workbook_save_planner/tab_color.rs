//! Worksheet tab color mutation, preserving other sheet properties.
use super::state::PlannerState;
use jscompat::TryJsRegex;
use serde_json::Value;
use std::sync::Arc;

const SHEET_PR: &str = r"<sheetPr\b[^>]*\/>|<sheetPr\b[^>]*>[\s\S]*?<\/sheetPr>";
const TAB_COLOR: &str = r"<tabColor\b[^>]*\/>|<tabColor\b[^>]*>[\s\S]*?<\/tabColor>";

fn replace_first(text: &str, pattern: &str, replacement: &str) -> Result<String, String> {
    let regex = TryJsRegex::new(pattern, "").map_err(|error| error.to_string())?;
    let Some(found) = regex.try_exec(text).map_err(|error| error.to_string())? else {
        return Ok(text.to_owned());
    };
    let mut out = String::with_capacity(text.len() + replacement.len());
    out.push_str(&text[..found.start()]);
    out.push_str(replacement);
    out.push_str(&text[found.end()..]);
    Ok(out)
}

fn apply_color(xml: &str, color: Option<&str>) -> Result<String, String> {
    let rgb = if let Some(color) = color {
        let hex = color.trim_matches(jscompat::is_js_space);
        let hex = hex.strip_prefix('#').unwrap_or(hex);
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("Unsupported tab color \"{color}\"."));
        }
        Some(format!("FF{}", hex.to_ascii_uppercase()))
    } else {
        None
    };
    let sheet_re = TryJsRegex::new(SHEET_PR, "").map_err(|error| error.to_string())?;
    let existing = sheet_re.try_exec(xml).map_err(|error| error.to_string())?;
    let Some(rgb) = rgb else {
        let Some(sheet) = existing else {
            return Ok(xml.to_owned());
        };
        let stripped = replace_first(sheet.as_str(), TAB_COLOR, "")?;
        let empty = TryJsRegex::new(r"^<sheetPr\b([^>]*)>\s*<\/sheetPr>$", "")
            .map_err(|error| error.to_string())?;
        let remove = empty
            .try_exec(&stripped)
            .map_err(|error| error.to_string())?
            .is_some_and(|found| {
                found
                    .get(1)
                    .unwrap_or("")
                    .trim_matches(jscompat::is_js_space)
                    .is_empty()
            });
        return replace_first(xml, SHEET_PR, if remove { "" } else { &stripped });
    };
    let element = format!("<tabColor rgb=\"{rgb}\"/>");
    let Some(sheet) = existing else {
        let root = TryJsRegex::new(r"<worksheet\b[^>]*>", "").map_err(|error| error.to_string())?;
        let Some(found) = root.try_exec(xml).map_err(|error| error.to_string())? else {
            return Ok(xml.to_owned());
        };
        let mut out = String::with_capacity(xml.len() + element.len() + 19);
        out.push_str(&xml[..found.end()]);
        out.push_str("<sheetPr>");
        out.push_str(&element);
        out.push_str("</sheetPr>");
        out.push_str(&xml[found.end()..]);
        return Ok(out);
    };
    let sheet = sheet.as_str();
    let tab_re = TryJsRegex::new(TAB_COLOR, "").map_err(|error| error.to_string())?;
    let next = if tab_re.try_test(sheet).map_err(|error| error.to_string())? {
        replace_first(sheet, TAB_COLOR, &element)?
    } else if let Some(open) = sheet.strip_suffix("/>") {
        format!("{open}>{element}</sheetPr>")
    } else {
        let open = TryJsRegex::new(r"<sheetPr\b[^>]*>", "").map_err(|error| error.to_string())?;
        let Some(found) = open.try_exec(sheet).map_err(|error| error.to_string())? else {
            return Ok(xml.to_owned());
        };
        format!(
            "{}{element}{}",
            &sheet[..found.end()],
            &sheet[found.end()..]
        )
    };
    replace_first(xml, SHEET_PR, &next)
}

pub(super) fn apply(state: &mut PlannerState, request: &Value) -> Result<(), String> {
    for item in request
        .get("tabColorStates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = item.get("sheetName").and_then(Value::as_str) else {
            continue;
        };
        let Some(sheet) = state.worksheet_mut(name) else {
            continue;
        };
        let color = item.get("color").and_then(Value::as_str);
        sheet.xml = Arc::new(apply_color(&sheet.xml, color)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::apply_color;
    #[test]
    fn replaces_removes_and_inserts_tab_color() {
        assert_eq!(
            apply_color(
                "<worksheet><sheetPr><outlinePr/></sheetPr></worksheet>",
                Some("#aabbcc")
            )
            .unwrap(),
            "<worksheet><sheetPr><tabColor rgb=\"FFAABBCC\"/><outlinePr/></sheetPr></worksheet>"
        );
        assert_eq!(
            apply_color(
                "<worksheet><sheetPr><tabColor rgb=\"FF000000\"/></sheetPr></worksheet>",
                None
            )
            .unwrap(),
            "<worksheet></worksheet>"
        );
        assert_eq!(
            apply_color(
                "<worksheet><sheetPr codeName=\"x\"/></worksheet>",
                Some("112233")
            )
            .unwrap(),
            "<worksheet><sheetPr codeName=\"x\"><tabColor rgb=\"FF112233\"/></sheetPr></worksheet>"
        );
        assert!(apply_color("<worksheet/>", Some("blue")).is_err());
    }
}
