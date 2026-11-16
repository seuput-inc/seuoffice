//! Selection-style overlay and PDFium-measured segment positioning.
use super::super::{field, number, text};
use super::fold::{fold, index_of};
use shell_contract::IpcValue;

fn rgb(value: Option<&IpcValue>) -> Option<[u8; 3]> {
    let IpcValue::Array(values) = value? else {
        return None;
    };
    let channel = |i| number(values.get(i)).map(|n| n as u8);
    Some([channel(0)?, channel(1)?, channel(2)?])
}
fn boolean(value: Option<&IpcValue>) -> Option<bool> {
    match value {
        Some(IpcValue::Bool(value)) => Some(*value),
        _ => None,
    }
}

pub(super) fn planned_styles(edit: &IpcValue, planned: &str) -> Option<Vec<Option<RunStyle>>> {
    let runs = field(edit, "styleRuns").or_else(|| field(edit, "colorRuns"))?;
    let IpcValue::Array(runs) = runs else {
        return None;
    };
    if runs.is_empty() {
        return None;
    }
    let user = fold(text(field(edit, "newText"))?, false);
    let target = fold(planned, false);
    let offset = index_of(&target.units, &user.units)?;
    let styles: Vec<_> = runs
        .iter()
        .map(|run| RunStyle {
            color: rgb(field(run, "color")),
            font: text(field(run, "font")).map(str::to_owned),
            size: number(field(run, "size")),
            bold: boolean(field(run, "bold")),
            italic: boolean(field(run, "italic")),
        })
        .collect();
    let mut out = vec![None; planned.encode_utf16().count()];
    for (i, &start) in user.start.iter().take(user.units.len()).enumerate() {
        let style = runs
            .iter()
            .position(|run| {
                let first = number(field(run, "start")).unwrap_or(f64::NAN);
                let end = number(field(run, "end")).unwrap_or(f64::NAN);
                (start as f64) >= first && (start as f64) < end
            })
            .map(|index| styles[index].clone());
        if let Some(style) = style {
            let p = i + offset;
            for slot in &mut out[target.start[p]..target.end[p]] {
                *slot = Some(style.clone());
            }
        }
    }
    let units: Vec<u16> = planned.encode_utf16().collect();
    for i in 1..out.len() {
        if out[i].is_none()
            && units[i] != b'\n' as u16
            && char::from_u32(units[i] as u32).is_some_and(jscompat::is_js_space)
        {
            out[i] = out[i - 1].clone();
        }
    }
    out.iter().any(Option::is_some).then_some(out)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct RunStyle {
    pub color: Option<[u8; 3]>,
    pub font: Option<String>,
    pub size: Option<f64>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
}

impl RunStyle {
    pub fn beyond_color(&self) -> bool {
        self.font.is_some() || self.size.is_some() || self.bold.is_some() || self.italic.is_some()
    }
}

pub(super) struct LineSegment {
    pub text: String,
    pub style: Option<RunStyle>,
    pub x_pt: f64,
}

/// Styles are indexed by UTF-16 code unit, as in the renderer's IPC contract.
pub(super) fn segment_line(
    line: &str,
    styles: Option<&[Option<RunStyle>]>,
    mut advance: impl FnMut(char, Option<&RunStyle>) -> Option<f64>,
) -> Vec<LineSegment> {
    let whole = || {
        vec![LineSegment {
            text: line.to_owned(),
            style: None,
            x_pt: 0.0,
        }]
    };
    let Some(styles) = styles else { return whole() };
    let mut groups: Vec<LineSegment> = Vec::new();
    let mut unit = 0;
    for ch in line.chars() {
        let style = styles.get(unit).cloned().flatten();
        if let Some(last) = groups.last_mut().filter(|last| last.style == style) {
            last.text.push(ch);
        } else {
            groups.push(LineSegment {
                text: ch.to_string(),
                style,
                x_pt: 0.0,
            });
        }
        unit += ch.len_utf16();
    }
    if groups.is_empty() {
        return whole();
    }
    if groups.len() == 1 {
        return groups;
    }
    let mut x = 0.0;
    for group in &mut groups {
        group.x_pt = x;
        for ch in group.text.chars() {
            let Some(width) = advance(ch, group.style.as_ref()) else {
                return whole();
            };
            x += width;
        }
    }
    groups
}

pub(super) fn overlay_styles(
    text: &str,
    colors: Option<&[Option<[u8; 3]>]>,
    overrides: Option<&[Option<RunStyle>]>,
) -> Option<Vec<Option<RunStyle>>> {
    if colors.is_none() && overrides.is_none() {
        return None;
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut result: Vec<Option<RunStyle>> = Vec::with_capacity(units.len());
    for index in 0..units.len() {
        let color = colors
            .and_then(|values| values.get(index))
            .copied()
            .flatten();
        let mut style = overrides
            .and_then(|values| values.get(index))
            .cloned()
            .flatten();
        match (&mut style, color) {
            (Some(style), Some(color)) if style.color.is_none() => style.color = Some(color),
            (None, Some(color)) => {
                style = Some(RunStyle {
                    color: Some(color),
                    ..Default::default()
                })
            }
            _ => {}
        }
        // JavaScript regex whitespace on a single UTF-16 code unit.
        if style.is_none()
            && index > 0
            && units[index] != b'\n' as u16
            && char::from_u32(u32::from(units[index])).is_some_and(jscompat::is_js_space)
        {
            style = result[index - 1].clone();
        }
        result.push(style);
    }
    result.iter().any(Option::is_some).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codepoint_advances_and_utf16_style_offsets() {
        let red = RunStyle {
            color: Some([255, 0, 0]),
            ..Default::default()
        };
        let styles = vec![None, Some(red.clone()), Some(red.clone()), Some(red)];
        let segments = segment_line("a😀b", Some(&styles), |_, _| Some(2.0));
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[1].text, "😀b");
        assert_eq!(segments[1].x_pt, 2.0);
    }

    #[test]
    fn missing_advance_reverts_to_original_uniform_line() {
        let styles = vec![None, Some(RunStyle::default())];
        let segments = segment_line("ab", Some(&styles), |_, _| None);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "ab");
        assert!(segments[0].style.is_none());
    }

    #[test]
    fn inherited_color_and_whitespace_follow_selection() {
        let colors = vec![Some([1, 2, 3]), None, None];
        let overrides = vec![
            Some(RunStyle {
                bold: Some(true),
                ..Default::default()
            }),
            None,
            None,
        ];
        let result = overlay_styles("a \n", Some(&colors), Some(&overrides)).unwrap();
        assert_eq!(result[0].as_ref().unwrap().color, Some([1, 2, 3]));
        assert_eq!(result[1], result[0]);
        assert_eq!(result[2], None);
    }
}
