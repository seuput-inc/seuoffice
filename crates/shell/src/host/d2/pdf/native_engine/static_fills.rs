//! Rebase the editor's cached static form fills after page edits.

use shell_contract::IpcValue;

fn field<'a>(value: &'a IpcValue, name: &str) -> Option<&'a IpcValue> {
    match value { IpcValue::Object(entries) => entries.iter().find(|(key, _)| key == name).map(|(_, value)| value), _ => None }
}
fn array(value: Option<&IpcValue>) -> &[IpcValue] {
    match value { Some(IpcValue::Array(values)) => values, _ => &[] }
}
fn number(value: Option<&IpcValue>) -> Option<f64> {
    match value { Some(IpcValue::Number(value)) => Some(*value), _ => None }
}
fn rectangle(value: Option<&IpcValue>) -> Option<[f64; 4]> {
    let values = array(value);
    if values.len() != 4 { return None; }
    Some([number(values.first())?, number(values.get(1))?, number(values.get(2))?, number(values.get(3))?])
}
fn intersects(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0].min(a[2]) < b[0].max(b[2]) && a[0].max(a[2]) > b[0].min(b[2]) &&
        a[1].min(a[3]) < b[1].max(b[3]) && a[1].max(a[3]) > b[1].min(b[3])
}

pub(super) fn resulting_fills(request: &IpcValue, page_count: usize) -> Option<IpcValue> {
    let fills = field(request, "staticFormFills")?;
    if matches!(fills, IpcValue::Undefined) { return None; }
    let deleted: Vec<f64> = array(field(request, "deletedPages")).iter().filter_map(|value| number(Some(value))).collect();
    let keep = |page: &f64| !deleted.contains(page);
    let remaining: Vec<f64> = match field(request, "pageOrder") {
        Some(IpcValue::Array(order)) => order.iter().filter_map(|value| number(Some(value))).filter(keep).collect(),
        _ => (0..page_count).map(|index| index as f64).filter(keep).collect(),
    };
    let mut output = Vec::new();
    for fill in array(Some(fills)) {
        let Some(page) = number(field(fill, "pageIndex")) else { continue };
        let covered = array(field(request, "redactions")).iter().any(|redaction| {
            number(field(redaction, "pageIndex")) == Some(page) &&
                rectangle(field(redaction, "rect")).zip(rectangle(field(fill, "rect"))).is_some_and(|(a, b)| intersects(a, b))
        });
        if covered { continue; }
        let Some(new_page) = remaining.iter().rposition(|old| *old == page) else { continue };
        let IpcValue::Object(entries) = fill else { continue };
        let mut copy = entries.clone();
        if let Some((_, value)) = copy.iter_mut().find(|(name, _)| name == "pageIndex") { *value = IpcValue::Number(new_page as f64); }
        output.push(IpcValue::Object(copy));
    }
    Some(IpcValue::Array(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn object(entries: Vec<(&str, IpcValue)>) -> IpcValue { IpcValue::Object(entries.into_iter().map(|(key, value)| (key.into(), value)).collect()) }
    fn rect(x: f64) -> IpcValue { IpcValue::Array(vec![IpcValue::Number(x), IpcValue::Number(0.0), IpcValue::Number(x + 1.0), IpcValue::Number(1.0)]) }
    #[test]
    fn redaction_drops_fill_and_page_order_remaps_the_rest() {
        let fill = |page, x| object(vec![("pageIndex", IpcValue::Number(page)), ("rect", rect(x))]);
        let request = object(vec![("staticFormFills", IpcValue::Array(vec![fill(0.0, 0.0), fill(1.0, 2.0)])), ("redactions", IpcValue::Array(vec![fill(0.0, 0.0)])), ("pageOrder", IpcValue::Array(vec![IpcValue::Number(1.0), IpcValue::Number(0.0)]))]);
        assert_eq!(resulting_fills(&request, 2), Some(IpcValue::Array(vec![fill(0.0, 2.0)])));
    }
}
