use std::collections::HashSet;

use super::{Context, Object, RefKey, field};
use shell_contract::pdf_sign_options::VisibleSignature;

/// Resolve pages in the same depth-first order as PDFPageTree.traverse.
pub(super) fn pages(context: &Context) -> Result<Vec<(f64, f64)>, String> {
    let root = context
        .root
        .as_ref()
        .and_then(|root| context.lookup(root))
        .ok_or("missing PDF catalog")?;
    let Object::Dictionary(root) = root else {
        return Err("invalid PDF catalog".into());
    };
    if !matches!(field(root, b"Type"), Some(Object::Name(name)) if name == b"Catalog") {
        return Err("invalid PDF catalog".into());
    }
    let pages = field(root, b"Pages").ok_or("missing PDF page tree")?;
    let mut result = Vec::new();
    let mut ancestors = HashSet::new();
    visit(context, pages, &mut ancestors, &mut result)?;
    Ok(result)
}

fn visit(
    context: &Context,
    reference: &Object,
    ancestors: &mut HashSet<RefKey>,
    output: &mut Vec<(f64, f64)>,
) -> Result<(), String> {
    let Object::Reference(number, generation) = reference else {
        return Err("invalid PDF page reference".into());
    };
    let key = RefKey::new(*number, *generation);
    if !ancestors.insert(key) {
        return Err("cyclic PDF page tree".into());
    }
    let Some(value) = context.lookup(reference) else {
        ancestors.remove(&key);
        return Ok(());
    };
    let Object::Dictionary(dict) = value else {
        ancestors.remove(&key);
        return Ok(());
    };
    match field(dict, b"Type") {
        Some(Object::Name(name)) if name == b"Page" => output.push((*number, *generation)),
        Some(Object::Name(name)) if name == b"Pages" => {
            let Some(Object::Array(kids)) = context.lookup_field(dict, b"Kids") else {
                return Err("invalid PDF page tree kids".into());
            };
            for kid in kids {
                visit(context, kid, ancestors, output)?;
            }
        }
        _ => {}
    }
    ancestors.remove(&key);
    Ok(())
}

fn inherited<'a>(
    context: &'a Context,
    page: (f64, f64),
    wanted: &[u8],
) -> Result<Option<&'a Object>, String> {
    let mut current = context
        .objects
        .get(&RefKey::new(page.0, page.1))
        .ok_or("missing PDF page ancestor")?;
    let mut seen = HashSet::new();
    let mut selected = false;
    let mut found = None;
    loop {
        if !seen.insert(current as *const Object) {
            return Err("cyclic PDF page ancestry".into());
        }
        let Object::Dictionary(dict) = current else {
            return Err("invalid PDF page ancestor".into());
        };
        if !selected
            && let Some(value) = field(dict, wanted)
        {
            selected = true;
            found = context.lookup(value);
        }
        current = match field(dict, b"Parent").and_then(|parent| context.lookup(parent)) {
            None | Some(Object::Null) => return Ok(found),
            Some(parent @ Object::Dictionary(dict)) if matches!(field(dict, b"Type"), Some(Object::Name(name)) if name == b"Pages") => {
                parent
            }
            _ => return Err("invalid PDF page parent".into()),
        };
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct Placement {
    pub(super) rect: [f64; 4],
    pub(super) rotation: f64,
    pub(super) width: f64,
    pub(super) height: f64,
}

/// Derive the signature box from trusted options and the original page.
pub(super) fn placement(
    context: &Context,
    page: (f64, f64),
    visible: &VisibleSignature,
) -> Result<Placement, String> {
    let crop = match inherited(context, page, b"CropBox")? {
        None | Some(Object::Null) => inherited(context, page, b"MediaBox")?,
        value => value,
    };
    let Some(Object::Array(crop)) = crop else {
        return Err("invalid PDF crop box".into());
    };
    if crop.len() != 4 {
        return Err("invalid PDF crop box".into());
    }
    let mut box_values = [0.0; 4];
    for (index, value) in crop.iter().enumerate() {
        let Some(Object::Number(number)) = context.lookup(value) else {
            return Err("invalid PDF crop box".into());
        };
        box_values[index] = *number;
    }
    let rotation = match inherited(context, page, b"Rotate")? {
        None | Some(Object::Null) => 0.0,
        Some(Object::Number(value)) => ((*value % 360.0) + 360.0) % 360.0,
        _ => return Err("invalid PDF page rotation".into()),
    };
    let page_width = box_values[2] - box_values[0];
    let page_height = box_values[3] - box_values[1];
    let sideways = rotation == 90.0 || rotation == 270.0;
    let view_width = if sideways { page_height } else { page_width };
    let view_height = if sideways { page_width } else { page_height };
    let width = visible.width.min(view_width);
    let height = visible.height.min(view_height);
    let margin = 36.0_f64.min(view_width / 10.0).min(view_height / 10.0);
    let left = if visible.position.ends_with("left") {
        margin
    } else if visible.position == "center" {
        (view_width - width) / 2.0
    } else {
        view_width - margin - width
    };
    let bottom = if visible.position.starts_with("top") {
        view_height - margin - height
    } else if visible.position == "center" {
        (view_height - height) / 2.0
    } else {
        margin
    };
    let map = |x: f64, y: f64| -> (f64, f64) {
        if rotation == 90.0 {
            (view_height - y, x)
        } else if rotation == 180.0 {
            (view_width - x, view_height - y)
        } else if rotation == 270.0 {
            (y, view_width - x)
        } else {
            (x, y)
        }
    };
    let (ax, ay) = map(left, bottom);
    let (bx, by) = map(left + width, bottom + height);
    Ok(Placement {
        rect: [
            box_values[0] + ax.min(bx),
            box_values[1] + ay.min(by),
            box_values[0] + ax.max(bx),
            box_values[1] + ay.max(by),
        ],
        rotation,
        width,
        height,
    })
}

/// Count valued signature fields and derive the exact new ASCII field name.
/// Non-ASCII PDFDocEncoding characters need only remain non-ASCII here: the
/// generated names consist exclusively of ASCII letters and decimal digits.
pub(super) fn new_field_name(context: &Context) -> Result<(usize, String), String> {
    fn text(value: Option<&Object>) -> Vec<u16> {
        let bytes = match value {
            Some(Object::String(raw)) => crate::host::d2::pdf::pdf_codec::literal_bytes(raw),
            Some(Object::HexString(raw)) => crate::host::d2::pdf::pdf_codec::hex_bytes(raw),
            _ => return Vec::new(),
        };
        if bytes.starts_with(&[0xfe, 0xff]) || bytes.starts_with(&[0xff, 0xfe]) {
            let little = bytes[0] == 0xff;
            let mut units = Vec::new();
            let mut index = 2;
            while index + 1 < bytes.len() {
                let pair = [bytes[index], bytes[index + 1]];
                let value = if little {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                };
                index += 2;
                if (0xd800..=0xdbff).contains(&value) {
                    if index + 1 < bytes.len() {
                        index += 2;
                    }
                    units.push(0xfffd);
                } else if (0xdc00..=0xdfff).contains(&value) {
                    index += 2;
                    units.push(0xfffd);
                } else {
                    units.push(value);
                }
            }
            if index < bytes.len() {
                units.push(0xfffd);
            }
            units
        } else {
            bytes
                .into_iter()
                .map(|byte| {
                    if (0x20..0x7f).contains(&byte) {
                        u16::from(byte)
                    } else {
                        0xfffd
                    }
                })
                .collect()
        }
    }
    fn visit(
        context: &Context,
        node: &Object,
        parent: &[u16],
        inherited: bool,
        seen: &mut HashSet<*const Object>,
        names: &mut Vec<Vec<u16>>,
    ) -> Result<(), String> {
        let object = match context.lookup(node) {
            None | Some(Object::Null) => return Ok(()),
            Some(object @ Object::Dictionary(_)) => object,
            _ => return Err("invalid signature field".into()),
        };
        let Object::Dictionary(dict) = object else {
            unreachable!()
        };
        if !seen.insert(object as *const Object) {
            return Ok(());
        }
        let partial = text(field(dict, b"T").and_then(|value| context.lookup(value)));
        let mut name = parent.to_vec();
        if !partial.is_empty() {
            if !name.is_empty() {
                name.push(u16::from(b'.'));
            }
            name.extend_from_slice(&partial);
        }
        let is_signature = match field(dict, b"FT").and_then(|value| context.lookup(value)) {
            Some(Object::Name(value)) => value == b"Sig",
            _ => inherited,
        };
        let value = match field(dict, b"V").and_then(|value| context.lookup(value)) {
            None | Some(Object::Null) => None,
            Some(Object::Dictionary(value)) => Some(value),
            _ => return Err("invalid signature field value".into()),
        };
        if is_signature && value.is_some_and(|value| field(value, b"ByteRange").is_some()) {
            names.push(name.clone());
        }
        match field(dict, b"Kids").and_then(|value| context.lookup(value)) {
            None | Some(Object::Null) => {}
            Some(Object::Array(kids)) => {
                for kid in kids {
                    visit(context, kid, &name, is_signature, seen, names)?;
                }
            }
            _ => return Err("invalid signature field kids".into()),
        }
        Ok(())
    }
    let root = context
        .root
        .as_ref()
        .and_then(|value| context.lookup(value))
        .ok_or("missing catalog")?;
    let Object::Dictionary(catalog) = root else {
        return Err("invalid catalog".into());
    };
    let mut names = Vec::new();
    match field(catalog, b"AcroForm").and_then(|value| context.lookup(value)) {
        None | Some(Object::Null) => {}
        Some(Object::Dictionary(form)) => {
            match field(form, b"Fields").and_then(|value| context.lookup(value)) {
                None | Some(Object::Null) => {}
                Some(Object::Array(fields)) => {
                    let mut seen = HashSet::new();
                    for field in fields {
                        visit(context, field, &[], false, &mut seen, &mut names)?;
                    }
                }
                _ => return Err("invalid signature fields".into()),
            }
        }
        _ => return Err("invalid PDF AcroForm".into()),
    }
    let mut counter = names.len() + 1;
    loop {
        let candidate = format!("Signature{counter}");
        if !names
            .iter()
            .any(|name| name.iter().copied().eq(candidate.encode_utf16()))
        {
            return Ok((names.len(), candidate));
        }
        counter += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::super::Parser;
    use super::*;

    #[test]
    fn finds_pages_in_depth_first_order() {
        let original = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R >> endobj\ntrailer << /Root 1 0 R >>\nstartxref\n0\n%%EOF\n";
        let old = Parser::new(original).document().unwrap();
        assert_eq!(pages(&old).unwrap(), vec![(3.0, 0.0)]);
    }

    #[test]
    fn derives_rotated_placement_from_inherited_page_geometry() {
        let input = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 100] /Rotate 90 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R >> endobj\ntrailer << /Root 1 0 R >>\nstartxref\n0\n%%EOF\n";
        let context = Parser::new(input).document().unwrap();
        let visible = VisibleSignature {
            page_index: 0.0,
            position: "bottom-right".into(),
            width: 40.0,
            height: 20.0,
            png: Vec::new(),
        };
        assert_eq!(
            placement(&context, (3.0, 0.0), &visible).unwrap(),
            Placement {
                rect: [170.0, 50.0, 190.0, 90.0],
                rotation: 90.0,
                width: 40.0,
                height: 20.0,
            }
        );
    }

    #[test]
    fn names_account_for_only_valued_signatures_and_utf16_names() {
        let input = b"%PDF-1.7\n1 0 obj << /Type /Catalog /AcroForm << /Fields [2 0 R 3 0 R 4 0 R] >> >> endobj\n2 0 obj << /FT /Sig /T <FEFF005300690067006E006100740075007200650033> /V << /ByteRange [0 1 2 3] >> >> endobj\n3 0 obj << /FT /Sig /T (Signature1) /V << /ByteRange [0 1 2 3] >> >> endobj\n4 0 obj << /FT /Sig /T (unsigned) >> endobj\ntrailer << /Root 1 0 R >>\nstartxref\n1\n%%EOF\n";
        let context = Parser::new(input).document().unwrap();
        assert_eq!(
            new_field_name(&context).unwrap(),
            (2, "Signature4".to_owned())
        );
    }
}
