//! Rewrite the workbook's theme palette and Latin font scheme.
use super::package::PackageEditor;
use jscompat::TryJsRegex;
use serde_json::Value;
use std::collections::HashSet;

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn replace(
    xml: &str,
    pattern: &str,
    replacement: &str,
    captured: bool,
) -> Result<Option<String>, String> {
    let re = TryJsRegex::new(pattern, "").map_err(|e| e.to_string())?;
    let Some(found) = re.try_exec(xml).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let value = if captured {
        format!(
            "{}{}{}",
            found.get(1).unwrap_or(""),
            replacement,
            found.get(2).unwrap_or("")
        )
    } else {
        replacement.to_owned()
    };
    Ok(Some(format!(
        "{}{}{}",
        &xml[..found.start()],
        value,
        &xml[found.end()..]
    )))
}
fn apply(xml: &str, state: &Value) -> Result<String, String> {
    let mut result = xml.to_owned();
    if let Some(colors) = state.get("colors") {
        let values = colors
            .get("values")
            .and_then(Value::as_array)
            .ok_or("A theme palette needs 12 colors.")?;
        if values.len() != 12 {
            return Err("A theme palette needs 12 colors.".into());
        }
        for (slot, index) in [
            ("dk1", 1),
            ("lt1", 0),
            ("dk2", 3),
            ("lt2", 2),
            ("accent1", 4),
            ("accent2", 5),
            ("accent3", 6),
            ("accent4", 7),
            ("accent5", 8),
            ("accent6", 9),
            ("hlink", 10),
            ("folHlink", 11),
        ] {
            let value = values[index]
                .as_str()
                .unwrap_or("")
                .strip_prefix('#')
                .unwrap_or(values[index].as_str().unwrap_or(""));
            if value.len() != 6 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("Theme color {slot} is not a valid #RRGGBB value."));
            }
            let pattern = format!(r"<a:{slot}>[\s\S]*?</a:{slot}>");
            let element = format!(
                "<a:{slot}><a:srgbClr val=\"{}\"/></a:{slot}>",
                value.to_ascii_uppercase()
            );
            result = replace(&result, &pattern, &element, false)?
                .ok_or_else(|| format!("The theme has no {slot} color scheme slot."))?;
        }
        let name = escape(colors.get("name").and_then(Value::as_str).unwrap_or(""));
        result =
            replace(&result, r#"(<a:clrScheme name=")[^"]*(")"#, &name, true)?.unwrap_or(result);
    }
    if let Some(fonts) = state.get("fonts") {
        let major = escape(fonts.get("major").and_then(Value::as_str).unwrap_or(""));
        let minor = escape(fonts.get("minor").and_then(Value::as_str).unwrap_or(""));
        result = replace(
            &result,
            r#"(<a:majorFont>[\s\S]*?<a:latin[^>]*typeface=")[^"]*(")"#,
            &major,
            true,
        )?
        .ok_or("The theme has no font scheme to rewrite.")?;
        result = replace(
            &result,
            r#"(<a:minorFont>[\s\S]*?<a:latin[^>]*typeface=")[^"]*(")"#,
            &minor,
            true,
        )?
        .ok_or("The theme has no font scheme to rewrite.")?;
        let name = escape(fonts.get("name").and_then(Value::as_str).unwrap_or(""));
        result =
            replace(&result, r#"(<a:fontScheme name=")[^"]*(")"#, &name, true)?.unwrap_or(result);
    }
    Ok(result)
}
pub(super) fn write(
    pkg: &mut PackageEditor<'_>,
    request: &Value,
    touched: &mut HashSet<String>,
) -> Result<(), String> {
    let Some(state) = request.get("themeState").filter(|state| !state.is_null()) else {
        return Ok(());
    };
    let path = "xl/theme/theme1.xml";
    if !pkg.has(path)? {
        return Err("The workbook has no theme part — theme changes cannot be saved.".into());
    }
    let original = pkg.read_text(path)?;
    pkg.write(path, apply(&original, state)?)?;
    touched.insert(path.into());
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_missing_font_scheme() {
        assert!(
            apply(
                "<a:theme/>",
                &serde_json::json!({"fonts":{"name":"x","major":"a","minor":"b"}})
            )
            .is_err()
        );
    }
    #[test]
    fn escapes_font_names() {
        let xml = "<a:fontScheme name=\"x\"><a:majorFont><a:latin typeface=\"a\"/></a:majorFont><a:minorFont><a:latin typeface=\"b\"/></a:minorFont></a:fontScheme>";
        let output = apply(
            xml,
            &serde_json::json!({"fonts":{"name":"N","major":"A&B","minor":"C"}}),
        )
        .unwrap();
        assert!(output.contains("typeface=\"A&amp;B\""));
    }
}
