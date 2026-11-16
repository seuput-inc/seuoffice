use html5ever::tendril::TendrilSink;
use html5ever::tokenizer::TokenizerOpts;
use html5ever::{ParseOpts, parse_document};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use serde_json::{Value, json};
use seu_host::HostError;

fn field<'a>(input: &'a Value, key: &str) -> Result<&'a str, HostError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| HostError {
            name: "TypeError".into(),
            message: format!("host step input has no string {key}"),
        })
}

fn document(text: &str) -> RcDom {
    let options = ParseOpts {
        tokenizer: TokenizerOpts {
            discard_bom: false,
            ..Default::default()
        },
        ..Default::default()
    };
    parse_document(RcDom::default(), options).one(text)
}

fn children(node: &Handle, templates: bool) -> Vec<Handle> {
    if templates {
        if let NodeData::Element {
            name,
            template_contents,
            ..
        } = &node.data
        {
            if &*name.local == "template" {
                if let Some(content) = template_contents.borrow().as_ref() {
                    return content.children.borrow().iter().cloned().collect();
                }
            }
        }
    }
    node.children.borrow().iter().cloned().collect()
}

fn flatten(roots: Vec<Handle>, xml: bool) -> Value {
    let mut order = Vec::<Handle>::new();
    let mut stack: Vec<Handle> = roots.iter().rev().cloned().collect();
    while let Some(node) = stack.pop() {
        if matches!(&node.data, NodeData::Element { .. }) {
            stack.extend(children(&node, xml).into_iter().rev());
            order.push(node);
        }
    }
    let ids: std::collections::HashMap<*const markup5ever_rcdom::Node, usize> = order
        .iter()
        .enumerate()
        .map(|(i, n)| (std::rc::Rc::as_ptr(n), i))
        .collect();
    let child_values = |nodes: Vec<Handle>| -> Vec<Value> {
        nodes
            .into_iter()
            .filter_map(|node| match &node.data {
                NodeData::Text { contents } => Some(Value::String(contents.borrow().to_string())),
                NodeData::Element { .. } => ids
                    .get(&std::rc::Rc::as_ptr(&node))
                    .map(|id| json!({"element": id})),
                _ => None,
            })
            .collect()
    };
    let elements: Vec<Value> = order.iter().map(|node| {
        let NodeData::Element { name, attrs, .. } = &node.data else { unreachable!() };
        let tag = name.local.to_string();
        let child = child_values(children(node, xml));
        if !xml { return json!({"name": tag, "children": child}); }
        let attributes: Vec<Value> = attrs.borrow().iter().map(|attr| {
            let local = attr.name.local.to_string();
            let qualified = match &attr.name.prefix {
                Some(prefix) => format!("{prefix}:{local}"),
                None => local.clone(),
            };
            let uri = if attr.name.ns.is_empty() { None } else { Some(attr.name.ns.to_string()) };
            json!({"name": qualified, "local": local.split_once(':').map_or(local.as_str(), |(_, tail)| tail), "uri": uri, "value": attr.value.to_string()})
        }).collect();
        json!({"name": tag, "local": tag.split_once(':').map_or(tag.as_str(), |(_, tail)| tail), "uri": null, "attributes": attributes, "children": child})
    }).collect();
    if xml {
        json!({"elements": elements})
    } else {
        json!({"elements": elements, "roots": child_values(roots)})
    }
}

pub(super) fn call(name: &str, input: &Value) -> Result<Value, HostError> {
    match name {
        "fileparse.diagramFormatOf" => Ok(json!(
            main_core::packages::file_parse::diagram::diagram_format_of(field(input, "filePath")?)
        )),
        "epub.parseFragmentText" => {
            let nodes = main_core::html_parse::parse_fragment(field(input, "html")?);
            Ok(match nodes.first() {
                Some(main_core::html_parse::Node::Text(text)) => json!(text),
                _ => Value::Null,
            })
        }
        "epub.parseHtml" => {
            let dom = document(field(input, "text")?);
            let root = dom.document.children.borrow().iter().find(|node| {
                matches!(&node.data, NodeData::Element { name, .. } if &*name.local == "html")
            }).cloned().ok_or_else(|| HostError { name: "Error".into(), message: "html: no root element".into() })?;
            Ok(flatten(vec![root], true))
        }
        "epub.chapterTextRoots" => {
            let dom = document(field(input, "text")?);
            let roots = dom.document.children.borrow().iter().cloned().collect();
            Ok(flatten(roots, false))
        }
        "fileparse.localOffsetSeconds" => {
            use chrono::{Local, TimeZone};
            let ms = input
                .get("ms")
                .map(|value| match value {
                    Value::Null => 0.0,
                    Value::Bool(flag) => f64::from(u8::from(*flag)),
                    Value::Number(number) => number.as_f64().unwrap_or(f64::NAN),
                    Value::String(text) => jscompat::to_number(text),
                    _ => f64::NAN,
                })
                .filter(|v| v.is_finite())
                .ok_or_else(|| HostError {
                    name: "TypeError".into(),
                    message: "host step input has no number ms".into(),
                })?;
            let offset = if ms.abs() <= 8.64e15 {
                Local
                    .timestamp_millis_opt(ms.trunc() as i64)
                    .single()
                    .map(|date| date.offset().local_minus_utc() / 60 * 60)
            } else {
                None
            };
            Ok(json!(offset))
        }
        _ => Err(HostError {
            name: "HostUnavailable".into(),
            message: format!("unknown host step: {name}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_document_repairs_implicit_elements_and_flattens_template() {
        let value = call(
            "epub.parseHtml",
            &json!({"text": "<title>😀</title><template><b>&amp;</b></template><p>x<p>y"}),
        )
        .unwrap();
        let elements = value["elements"].as_array().unwrap();
        let names: Vec<_> = elements
            .iter()
            .map(|item| item["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["html", "head", "title", "template", "b", "body", "p", "p"]
        );
        assert_eq!(elements[2]["children"], json!(["😀"]));
        assert_eq!(elements[4]["children"], json!(["&"]));
    }

    #[test]
    fn chapter_template_contents_are_not_children() {
        let value = call(
            "epub.chapterTextRoots",
            &json!({"text": "<template><b>hidden</b></template><p>shown"}),
        )
        .unwrap();
        let elements = value["elements"].as_array().unwrap();
        let template = elements
            .iter()
            .find(|item| item["name"] == "template")
            .unwrap();
        assert_eq!(template["children"], json!([]));
        assert!(!elements.iter().any(|item| item["name"] == "b"));
    }

    #[test]
    fn fragment_decodes_entities_and_reports_non_text() {
        assert_eq!(
            call(
                "epub.parseFragmentText",
                &json!({"html": "&NotEqualTilde;😀"})
            )
            .unwrap(),
            json!("≂̸😀")
        );
        assert_eq!(
            call("epub.parseFragmentText", &json!({"html": "<b>x</b>"})).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn diagram_names_and_bad_fields() {
        assert_eq!(
            call(
                "fileparse.diagramFormatOf",
                &json!({"filePath": "/a/B.DRAWIO.SVG"})
            )
            .unwrap(),
            json!("drawio.svg")
        );
        assert_eq!(
            call(
                "fileparse.diagramFormatOf",
                &json!({"filePath": "book.vsdx"})
            )
            .unwrap(),
            Value::Null
        );
        assert_eq!(
            call("epub.parseHtml", &json!({})).unwrap_err().name,
            "TypeError"
        );
    }
}
