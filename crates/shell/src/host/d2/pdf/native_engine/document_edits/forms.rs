//! Page-rotation-aware placement and field naming for authored PDF widgets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct WidgetBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
pub(super) fn widget_box(rect: [f64; 4], rotation: f64) -> WidgetBox {
    let x = rect[0].min(rect[2]);
    let y = rect[1].min(rect[3]);
    let w = (rect[2] - rect[0]).abs();
    let h = (rect[3] - rect[1]).abs();
    let rot = ((rotation % 360.0) + 360.0) % 360.0;
    if rot == 90.0 {
        WidgetBox {
            x: x + w - 0.5,
            y: y + 0.5,
            width: h - 1.0,
            height: w - 1.0,
        }
    } else if rot == 180.0 {
        WidgetBox {
            x: x + w - 0.5,
            y: y + h - 0.5,
            width: w - 1.0,
            height: h - 1.0,
        }
    } else if rot == 270.0 {
        WidgetBox {
            x: x + 0.5,
            y: y + h - 0.5,
            width: h - 1.0,
            height: w - 1.0,
        }
    } else {
        WidgetBox {
            x: x + 0.5,
            y: y + 0.5,
            width: w - 1.0,
            height: h - 1.0,
        }
    }
}
pub(super) fn date_format(format: &str) -> String {
    format.chars().filter(|c| *c != '"' && *c != '\\').collect()
}

pub(super) fn apply_values(
    session: &mut super::session::Session,
    values: &[shell_contract::IpcValue],
) -> Result<(), String> {
    use serde_json::json;
    use shell_contract::IpcValue;
    fn field<'a>(v: &'a IpcValue, key: &str) -> Option<&'a IpcValue> {
        match v {
            IpcValue::Object(o) => o.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn text(v: Option<&IpcValue>) -> Option<&str> {
        match v {
            Some(IpcValue::String(s)) => Some(s),
            _ => None,
        }
    }
    for value in values {
        let name = text(field(value, "name")).unwrap_or("");
        let kind = text(field(value, "kind")).unwrap_or("");
        let data = text(field(value, "value"));
        let command = match kind {
            "text" => json!({"op":"formText","name":name,"value":data.unwrap_or("")}),
            "radio" => json!({"op":"formRadio","name":name,"value":data.filter(|s|!s.is_empty())}),
            "choice" => {
                json!({"op":"formChoice","name":name,"value":data.filter(|s|!s.is_empty())})
            }
            _ => {
                json!({"op":"formCheckbox","name":name,"checked":matches!(field(value,"checked"),Some(IpcValue::Bool(true)))})
            }
        };
        session.exchange(command)?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotations_account_for_border_padding() {
        assert_eq!(
            widget_box([0.0, 0.0, 20.0, 10.0], 90.0),
            WidgetBox {
                x: 19.5,
                y: 0.5,
                width: 9.0,
                height: 19.0
            }
        );
        assert_eq!(
            widget_box([20.0, 10.0, 0.0, 0.0], -90.0),
            WidgetBox {
                x: 0.5,
                y: 9.5,
                width: 9.0,
                height: 19.0
            }
        );
        assert_eq!(date_format("mm/\"dd\\yyyy"), "mm/ddyyyy");
    }
}
