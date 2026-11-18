//! Source-preserving PDF table cell classification for spreadsheet rebuilds.
use jscompat::js_regex;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum ParsedCell<'a> {
    Text(&'a str),
    Number { value: f64, number_format: String },
}

fn decimal_digits(text: &str) -> usize { text.find('.').map_or(0, |dot| text.len() - dot - 1) }
fn significant_digits(text: &str) -> usize { text.bytes().filter(|b| !matches!(b, b'-' | b',' | b'.')).skip_while(|b| *b == b'0').count() }
fn bare(text: &str) -> Option<(f64, bool)> {
    let plain = js_regex!(r"^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?$").test(text);
    let grouped = !plain && js_regex!(r"^-?[1-9][0-9]{0,2}(,[0-9]{3})+(\.[0-9]+)?$").test(text);
    if !plain && !grouped { return None; }
    if !text.contains('.') && !text.contains('e') && !text.contains('E') && significant_digits(text) > 15 { return None; }
    let value = if grouped { text.replace(',', "").parse::<f64>().ok()? } else { text.parse::<f64>().ok()? };
    value.is_finite().then_some((value, grouped))
}
fn currency_format(symbol: &str, decimal: bool) -> String {
    let digits = if decimal { "#,##0.00" } else { "#,##0" };
    match symbol { "元" => format!("{digits}\"元\""), "￥" | "¥" => format!("\"¥\"{digits}"), _ => format!("\"{symbol}\"{digits}") }
}

pub(super) fn parse_cell_value(raw: &str) -> ParsedCell<'_> {
    let text = jscompat::trim(raw);
    let as_text = || ParsedCell::Text(raw);
    if text.is_empty() { return as_text(); }
    if let Some((value, grouped)) = bare(text) {
        let format = if grouped { if text.contains('.') { "#,##0.00".to_owned() } else { "#,##0".to_owned() } }
        else if let Some(m) = js_regex!(r"\.([0-9]*0)$").exec(text) { format!("0.{}", "0".repeat(m.get(1).unwrap_or("").len())) }
        else { "General".to_owned() };
        return ParsedCell::Number { value, number_format: format };
    }
    if let Some(m) = js_regex!(r"^(-?[0-9.,]+)\s?%$").exec(text) {
        let source = m.get(1).unwrap_or("");
        if let Some((value, _)) = bare(source) {
            let decimals = decimal_digits(source);
            let precision = (decimals + 2).min(100);
            let rounded = format!("{:.*}", precision, value / 100.0).parse::<f64>().unwrap_or(value / 100.0);
            let format = if decimals == 0 { "0%".into() } else { format!("0.{}%", "0".repeat(decimals)) };
            return ParsedCell::Number { value: rounded, number_format: format };
        }
        return as_text();
    }
    if let Some(m) = js_regex!(r"^(-?)([$¥￥€£])\s?(-?[0-9.,]+)$").exec(text) {
        let sign = m.get(1).unwrap_or(""); let symbol = m.get(2).unwrap_or(""); let source = m.get(3).unwrap_or("");
        if sign == "-" && source.starts_with('-') { return as_text(); }
        if let Some((value, _)) = bare(source) { return ParsedCell::Number { value: if sign == "-" { -value } else { value }, number_format: currency_format(symbol, decimal_digits(source) > 0) }; }
        return as_text();
    }
    if let Some(m) = js_regex!(r"^(-?[0-9.,]+)\s?(元|€|\$)$").exec(text) {
        let source = m.get(1).unwrap_or("");
        if let Some((value, _)) = bare(source) { return ParsedCell::Number { value, number_format: currency_format(m.get(2).unwrap_or(""), decimal_digits(source) > 0) }; }
    }
    as_text()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn number(value: f64, number_format: &str) -> ParsedCell<'static> { ParsedCell::Number { value, number_format: number_format.into() } }
    #[test]
    fn preserves_excel_types_and_authored_precision() {
        assert_eq!(parse_cell_value("31.10"), number(31.1, "0.00"));
        assert_eq!(parse_cell_value("1,234.56"), number(1234.56, "#,##0.00"));
        assert_eq!(parse_cell_value("45.3%"), number(0.453, "0.0%"));
        assert_eq!(parse_cell_value("-$45"), number(-45.0, "\"$\"#,##0"));
        assert_eq!(parse_cell_value("￥1,234.50"), number(1234.5, "\"¥\"#,##0.00"));
        assert_eq!(parse_cell_value(" -$-45 "), ParsedCell::Text(" -$-45 "));
        assert_eq!(parse_cell_value("00123"), ParsedCell::Text("00123"));
        assert_eq!(parse_cell_value("1234567890123456"), ParsedCell::Text("1234567890123456"));
    }
}
