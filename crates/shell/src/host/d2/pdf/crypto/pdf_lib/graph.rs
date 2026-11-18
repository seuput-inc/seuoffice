use std::collections::{HashMap, HashSet};

use super::{Context, Object, RefKey, field};
use shell_contract::pdf_sign_options::{SignOptions, VisibleSignature};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Change {
    pub(super) reference: (f64, f64),
    pub(super) before: Option<Object>,
    pub(super) after: Option<Object>,
}

/// Compare the contexts produced by pdf-lib's sequential assignment rules.
pub(super) fn changes(original: &Context, revised: &Context) -> Vec<Change> {
    let mut keys = original.order.clone();
    let mut seen: HashSet<RefKey> = keys.iter().copied().collect();
    for key in &revised.order {
        if seen.insert(*key) { keys.push(*key); }
    }
    keys.into_iter().filter_map(|key| {
        let before = original.objects.get(&key);
        let after = revised.objects.get(&key);
        if before == after { return None; }
        Some(Change {
            reference: (f64::from_bits(key.0), f64::from_bits(key.1)),
            before: before.cloned(),
            after: after.cloned(),
        })
    }).collect()
}

/// Resolve pages in the same depth-first order as PDFPageTree.traverse.
pub(super) fn pages(context: &Context) -> Result<Vec<(f64, f64)>, String> {
    let root = context.root.as_ref().and_then(|root| context.lookup(root)).ok_or("missing PDF catalog")?;
    let Object::Dictionary(root) = root else { return Err("invalid PDF catalog".into()) };
    if !matches!(field(root, b"Type"), Some(Object::Name(name)) if name == b"Catalog") {
        return Err("invalid PDF catalog".into());
    }
    let pages = field(root, b"Pages").ok_or("missing PDF page tree")?;
    let mut result = Vec::new();
    let mut ancestors = HashSet::new();
    visit(context, pages, &mut ancestors, &mut result)?;
    Ok(result)
}

fn visit(context: &Context, reference: &Object, ancestors: &mut HashSet<RefKey>, output: &mut Vec<(f64, f64)>) -> Result<(), String> {
    let Object::Reference(number, generation) = reference else { return Err("invalid PDF page reference".into()) };
    let key = RefKey::new(*number, *generation);
    if !ancestors.insert(key) { return Err("cyclic PDF page tree".into()); }
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
            for kid in kids { visit(context, kid, ancestors, output)?; }
        }
        _ => {}
    }
    ancestors.remove(&key);
    Ok(())
}

fn inherited<'a>(context: &'a Context, page: (f64, f64), wanted: &[u8]) -> Result<Option<&'a Object>, String> {
    let mut current = context.objects.get(&RefKey::new(page.0, page.1)).ok_or("missing PDF page ancestor")?;
    let mut seen = HashSet::new();
    let mut selected = false;
    let mut found = None;
    loop {
        if !seen.insert(current as *const Object) { return Err("cyclic PDF page ancestry".into()); }
        let Object::Dictionary(dict) = current else { return Err("invalid PDF page ancestor".into()) };
        if !selected {
            if let Some(value) = field(dict, wanted) {
                selected = true;
                found = context.lookup(value);
            }
        }
        current = match field(dict, b"Parent").and_then(|parent| context.lookup(parent)) {
            None | Some(Object::Null) => return Ok(found),
            Some(parent @ Object::Dictionary(dict)) if matches!(field(dict, b"Type"), Some(Object::Name(name)) if name == b"Pages") => parent,
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
pub(super) fn placement(context: &Context, page: (f64, f64), visible: &VisibleSignature) -> Result<Placement, String> {
    let crop = match inherited(context, page, b"CropBox")? {
        None | Some(Object::Null) => inherited(context, page, b"MediaBox")?,
        value => value,
    };
    let Some(Object::Array(crop)) = crop else { return Err("invalid PDF crop box".into()) };
    if crop.len() != 4 { return Err("invalid PDF crop box".into()); }
    let mut box_values = [0.0; 4];
    for (index, value) in crop.iter().enumerate() {
        let Some(Object::Number(number)) = context.lookup(value) else { return Err("invalid PDF crop box".into()) };
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
    let left = if visible.position.ends_with("left") { margin }
        else if visible.position == "center" { (view_width - width) / 2.0 }
        else { view_width - margin - width };
    let bottom = if visible.position.starts_with("top") { view_height - margin - height }
        else if visible.position == "center" { (view_height - height) / 2.0 }
        else { margin };
    let map = |x: f64, y: f64| -> (f64, f64) {
        if rotation == 90.0 { (view_height - y, x) }
        else if rotation == 180.0 { (view_width - x, view_height - y) }
        else if rotation == 270.0 { (y, view_width - x) }
        else { (x, y) }
    };
    let (ax, ay) = map(left, bottom);
    let (bx, by) = map(left + width, bottom + height);
    Ok(Placement {
        rect: [
            box_values[0] + ax.min(bx), box_values[1] + ay.min(by),
            box_values[0] + ax.max(bx), box_values[1] + ay.max(by),
        ],
        rotation, width, height,
    })
}

fn pdf_text(value: &str) -> Object {
    if value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        let mut raw = Vec::with_capacity(value.len());
        for byte in value.bytes() {
            if matches!(byte, b'\\' | b'(' | b')') { raw.push(b'\\'); }
            raw.push(byte);
        }
        Object::String(raw)
    } else {
        let mut raw = b"FEFF".to_vec();
        for unit in value.encode_utf16() {
            raw.extend_from_slice(format!("{unit:04X}").as_bytes());
        }
        Object::HexString(raw)
    }
}

fn dictionary(entries: impl IntoIterator<Item = (&'static [u8], Object)>) -> Object {
    Object::Dictionary(entries.into_iter().map(|(name, value)| (name.to_vec(), value)).collect())
}

fn equivalent(left: &Object, right: &Object) -> bool {
    match (left, right) {
        (Object::Dictionary(a), Object::Dictionary(b)) => a.len() == b.len()
            && a.iter().all(|(key, value)| field(b, key).is_some_and(|other| equivalent(value, other))),
        (Object::Array(a), Object::Array(b)) => a.len() == b.len()
            && a.iter().zip(b).all(|(left, right)| equivalent(left, right)),
        _ => left == right,
    }
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
                let value = if little { u16::from_le_bytes(pair) } else { u16::from_be_bytes(pair) };
                index += 2;
                if (0xd800..=0xdbff).contains(&value) {
                    if index + 1 < bytes.len() { index += 2; }
                    units.push(0xfffd);
                } else if (0xdc00..=0xdfff).contains(&value) {
                    index += 2;
                    units.push(0xfffd);
                } else { units.push(value); }
            }
            if index < bytes.len() { units.push(0xfffd); }
            units
        } else {
            bytes.into_iter().map(|byte| if (0x20..0x7f).contains(&byte) { u16::from(byte) } else { 0xfffd }).collect()
        }
    }
    fn visit(context: &Context, node: &Object, parent: &[u16], inherited: bool, seen: &mut HashSet<*const Object>, names: &mut Vec<Vec<u16>>) -> Result<(), String> {
        let object = match context.lookup(node) {
            None | Some(Object::Null) => return Ok(()),
            Some(object @ Object::Dictionary(_)) => object,
            _ => return Err("invalid signature field".into()),
        };
        let Object::Dictionary(dict) = object else { unreachable!() };
        if !seen.insert(object as *const Object) { return Ok(()); }
        let partial = text(field(dict, b"T").and_then(|value| context.lookup(value)));
        let mut name = parent.to_vec();
        if !partial.is_empty() {
            if !name.is_empty() { name.push(u16::from(b'.')); }
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
                for kid in kids { visit(context, kid, &name, is_signature, seen, names)?; }
            }
            _ => return Err("invalid signature field kids".into()),
        }
        Ok(())
    }
    let root = context.root.as_ref().and_then(|value| context.lookup(value)).ok_or("missing catalog")?;
    let Object::Dictionary(catalog) = root else { return Err("invalid catalog".into()) };
    let mut names = Vec::new();
    match field(catalog, b"AcroForm").and_then(|value| context.lookup(value)) {
        None | Some(Object::Null) => {}
        Some(Object::Dictionary(form)) => match field(form, b"Fields").and_then(|value| context.lookup(value)) {
            None | Some(Object::Null) => {}
            Some(Object::Array(fields)) => {
                let mut seen = HashSet::new();
                for field in fields { visit(context, field, &[], false, &mut seen, &mut names)?; }
            }
            _ => return Err("invalid signature fields".into()),
        },
        _ => return Err("invalid PDF AcroForm".into()),
    }
    let mut counter = names.len() + 1;
    loop {
        let candidate = format!("Signature{counter}");
        if !names.iter().any(|name| name.iter().copied().eq(candidate.encode_utf16())) {
            return Ok((names.len(), candidate));
        }
        counter += 1;
    }
}

/// Check the native request's metadata against the hand-built signature object.
pub(super) fn signature_dictionary(
    actual: &Object,
    options: &SignOptions,
    signing_time: &str,
    range: [usize; 4],
) -> Result<(), String> {
    let placeholder = range[2].checked_sub(range[1]).and_then(|value| value.checked_sub(2))
        .ok_or("invalid signature placeholder")?;
    let name = |value: &[u8]| Object::Name(value.to_vec());
    let mut entries = vec![
        (b"Type".to_vec(), name(b"Sig")),
        (b"Filter".to_vec(), name(b"Adobe.PPKLite")),
        (b"SubFilter".to_vec(), name(b"adbe.pkcs7.detached")),
    ];
    if let Some(level) = options.certify_level {
        let params = dictionary([
            (b"Type".as_slice(), name(b"TransformParams")),
            (b"P".as_slice(), Object::Number(f64::from(level))),
            (b"V".as_slice(), name(b"1.2")),
        ]);
        let reference = dictionary([
            (b"Type".as_slice(), name(b"SigRef")),
            (b"TransformMethod".as_slice(), name(b"DocMDP")),
            (b"DigestMethod".as_slice(), name(b"SHA256")),
            (b"TransformParams".as_slice(), params),
        ]);
        entries.push((b"Reference".to_vec(), Object::Array(vec![reference])));
    }
    for (key, value) in [
        (b"Reason".as_slice(), options.reason.as_deref()),
        (b"Location".as_slice(), options.location.as_deref()),
        (b"ContactInfo".as_slice(), options.contact_info.as_deref()),
    ] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            entries.push((key.to_vec(), pdf_text(value)));
        }
    }
    entries.push((b"M".to_vec(), Object::String(signing_time.as_bytes().to_vec())));
    entries.push((b"ByteRange".to_vec(), Object::Array(range.into_iter().map(|value| Object::Number(value as f64)).collect())));
    entries.push((b"Contents".to_vec(), Object::HexString(vec![b'0'; placeholder])));
    if equivalent(actual, &Object::Dictionary(entries)) { Ok(()) }
    else { Err("signature dictionary does not match the native request".into()) }
}

/// Validate the new widget and return its appearance reference when visible.
pub(super) fn widget_dictionary(
    actual: &Object,
    field_name: &str,
    signature: (f64, f64),
    page: (f64, f64),
    placement: Option<&Placement>,
) -> Result<Option<(f64, f64)>, String> {
    let Object::Dictionary(actual_dict) = actual else { return Err("invalid signature widget".into()) };
    let name = |value: &[u8]| Object::Name(value.to_vec());
    let rect = placement.map_or([0.0; 4], |placement| placement.rect);
    let mut expected = vec![
        (b"Type".to_vec(), name(b"Annot")),
        (b"Subtype".to_vec(), name(b"Widget")),
        (b"FT".to_vec(), name(b"Sig")),
        (b"T".to_vec(), Object::String(field_name.as_bytes().to_vec())),
        (b"V".to_vec(), Object::Reference(signature.0, signature.1)),
        (b"F".to_vec(), Object::Number(132.0)),
        (b"Rect".to_vec(), Object::Array(rect.into_iter().map(Object::Number).collect())),
        (b"P".to_vec(), Object::Reference(page.0, page.1)),
    ];
    let appearance = if placement.is_some() {
        let Some(Object::Dictionary(ap)) = field(actual_dict, b"AP") else { return Err("missing signature appearance".into()) };
        let Some(Object::Reference(number, generation)) = field(ap, b"N") else { return Err("invalid signature appearance".into()) };
        let reference = (*number, *generation);
        expected.push((b"AP".to_vec(), dictionary([(b"N".as_slice(), Object::Reference(reference.0, reference.1))])));
        Some(reference)
    } else { None };
    if !equivalent(actual, &Object::Dictionary(expected)) { return Err("signature widget does not match the native request".into()); }
    Ok(appearance)
}

/// Validate the four form XObjects generated around the trusted PNG and
/// return the image reference for independent pixel validation.
pub(super) fn appearance_forms(
    context: &Context,
    appearance: (f64, f64),
    placed: &Placement,
) -> Result<(f64, f64), String> {
    fn form<'a>(context: &'a Context, reference: (f64, f64), bbox: [f64; 4], matrix: Option<[f64; 6]>, names: &[&[u8]]) -> Result<(&'a [u8], Vec<(f64, f64)>), String> {
        let Object::Stream(dict, body) = context.objects.get(&RefKey::new(reference.0, reference.1)).ok_or("missing appearance form")? else {
            return Err("appearance is not a stream".into());
        };
        let Some(Object::Dictionary(resources)) = field(dict, b"Resources") else { return Err("invalid form resources".into()) };
        let mut references = Vec::new();
        let expected_resources = if names.is_empty() {
            dictionary([])
        } else {
            let Some(Object::Dictionary(objects)) = field(resources, b"XObject") else { return Err("invalid form XObjects".into()) };
            let mut entries = Vec::new();
            for name in names {
                let Some(Object::Reference(number, generation)) = field(objects, name) else { return Err("missing form XObject".into()) };
                references.push((*number, *generation));
                entries.push((name.to_vec(), Object::Reference(*number, *generation)));
            }
            Object::Dictionary(vec![(b"XObject".to_vec(), Object::Dictionary(entries))])
        };
        let mut expected = vec![
            (b"Type".to_vec(), Object::Name(b"XObject".to_vec())),
            (b"Subtype".to_vec(), Object::Name(b"Form".to_vec())),
            (b"BBox".to_vec(), Object::Array(bbox.into_iter().map(Object::Number).collect())),
            (b"Resources".to_vec(), expected_resources),
            (b"Length".to_vec(), Object::Number(body.len() as f64)),
        ];
        if let Some(matrix) = matrix {
            expected.push((b"Matrix".to_vec(), Object::Array(matrix.into_iter().map(Object::Number).collect())));
        }
        if !equivalent(&Object::Dictionary(dict.clone()), &Object::Dictionary(expected)) {
            return Err("appearance form dictionary differs from native operation".into());
        }
        Ok((body, references))
    }
    let bbox = [0.0, 0.0, placed.width, placed.height];
    let matrix = if placed.rotation == 0.0 { None } else {
        let radians = placed.rotation * std::f64::consts::PI / 180.0;
        let cos = (radians.cos() + 0.5).floor();
        let sin = (radians.sin() + 0.5).floor();
        Some([cos, sin, -sin, cos, 0.0, 0.0])
    };
    let (top, layers_ref) = form(context, appearance, bbox, matrix, &[b"FRM"])?;
    if top != b"/FRM Do" { return Err("unexpected appearance program".into()); }
    let (layers, children) = form(context, layers_ref[0], bbox, None, &[b"n0", b"n2"])?;
    if layers != b"q 1 0 0 1 0 0 cm /n0 Do Q q 1 0 0 1 0 0 cm /n2 Do Q" {
        return Err("unexpected appearance layers".into());
    }
    let (blank, _) = form(context, children[0], [0.0, 0.0, 100.0, 100.0], None, &[])?;
    if blank != b"% DSBlank" { return Err("unexpected blank appearance".into()); }
    let (paint, image) = form(context, children[1], bbox, None, &[b"Sig0"])?;
    let parts: Vec<&[u8]> = paint.split(|byte| *byte == b' ').collect();
    if parts.len() != 11 || parts[0] != b"q" || parts[2] != b"0" || parts[3] != b"0"
        || parts[5] != b"0" || parts[6] != b"0" || parts[7] != b"cm"
        || parts[8] != b"/Sig0" || parts[9] != b"Do" || parts[10] != b"Q"
        || std::str::from_utf8(parts[1]).ok().and_then(|s| s.parse::<f64>().ok()) != Some(placed.width)
        || std::str::from_utf8(parts[4]).ok().and_then(|s| s.parse::<f64>().ok()) != Some(placed.height)
    {
        return Err("unexpected image paint program".into());
    }
    let refs = [appearance, layers_ref[0], children[0], children[1], image[0]];
    let distinct: HashSet<RefKey> = refs.into_iter().map(|reference| RefKey::new(reference.0, reference.1)).collect();
    if distinct.len() != refs.len() { return Err("aliased appearance objects".into()); }
    Ok(image[0])
}

/// Compare the embedded image and optional soft mask with pixels decoded from
/// the request-owned PNG, rather than trusting image bytes from the service.
pub(super) fn appearance_image(context: &Context, reference: (f64, f64), png: &[u8]) -> Result<Option<(f64, f64)>, String> {
    fn image_stream<'a>(context: &'a Context, reference: (f64, f64)) -> Result<(&'a [(Vec<u8>, Object)], &'a [u8]), String> {
        match context.objects.get(&RefKey::new(reference.0, reference.1)) {
            Some(Object::Stream(dict, body)) => Ok((dict, body)),
            _ => Err("missing signature image stream".into()),
        }
    }
    fn check_dict(dict: &[(Vec<u8>, Object)], body: &[u8], width: u32, height: u32, gray: bool, mask: Option<(f64, f64)>) -> Result<(), String> {
        let mut expected = vec![
            (b"Type".to_vec(), Object::Name(b"XObject".to_vec())),
            (b"Subtype".to_vec(), Object::Name(b"Image".to_vec())),
            (b"BitsPerComponent".to_vec(), Object::Number(8.0)),
            (b"Width".to_vec(), Object::Number(f64::from(width))),
            (b"Height".to_vec(), Object::Number(f64::from(height))),
            (b"ColorSpace".to_vec(), Object::Name(if gray { b"DeviceGray".to_vec() } else { b"DeviceRGB".to_vec() })),
            (b"Filter".to_vec(), Object::Name(b"FlateDecode".to_vec())),
            (b"Length".to_vec(), Object::Number(body.len() as f64)),
        ];
        if gray {
            expected.push((b"Decode".to_vec(), Object::Array(vec![Object::Number(0.0), Object::Number(1.0)])));
        }
        if let Some(mask) = mask {
            expected.push((b"SMask".to_vec(), Object::Reference(mask.0, mask.1)));
        }
        if !equivalent(&Object::Dictionary(dict.to_vec()), &Object::Dictionary(expected)) {
            return Err("signature image dictionary differs from native PNG".into());
        }
        Ok(())
    }
    let (width, height, image) = super::upng::rgba_png(png)?;
    let width = u32::try_from(width).map_err(|_| "invalid PNG width")?;
    let height = u32::try_from(height).map_err(|_| "invalid PNG height")?;
    let mut rgb = Vec::with_capacity(image.len() / 4 * 3);
    let mut alpha = Vec::with_capacity(image.len() / 4);
    for pixel in image.chunks_exact(4) {
        rgb.extend_from_slice(&pixel[..3]);
        alpha.push(pixel[3]);
    }
    let (dict, body) = image_stream(context, reference)?;
    let mask = if alpha.iter().any(|value| *value < 255) {
        match field(dict, b"SMask") {
            Some(Object::Reference(number, generation)) => Some((*number, *generation)),
            _ => return Err("missing signature image alpha mask".into()),
        }
    } else { None };
    check_dict(dict, body, width, height, false, mask)?;
    if super::stream::decode(context, dict, body)? != rgb { return Err("signature image pixels differ from native PNG".into()); }
    if let Some(mask) = mask {
        if RefKey::new(mask.0, mask.1) == RefKey::new(reference.0, reference.1) { return Err("aliased signature image mask".into()); }
        let (mask_dict, mask_body) = image_stream(context, mask)?;
        check_dict(mask_dict, mask_body, width, height, true, None)?;
        if super::stream::decode(context, mask_dict, mask_body)? != alpha { return Err("signature alpha pixels differ from native PNG".into()); }
    }
    Ok(mask)
}

/// Reproduce the mutations of pre-existing objects made by `assemble`.
/// Fresh signature, widget, image, appearance and xref objects must be
/// validated separately; this function does not authorize a signature.
pub(super) fn validate_existing_edits(
    original: &Context,
    revised: &Context,
    page: (f64, f64),
    widget: (f64, f64),
    signature: (f64, f64),
    certify: bool,
) -> Result<HashSet<RefKey>, String> {
    fn edit_dict(context: &Context, value: Option<&Object>) -> Result<Option<(Option<(f64, f64)>, Vec<(Vec<u8>, Object)>)>, String> {
        let Some(value) = value else { return Ok(None) };
        match value {
            Object::Reference(number, generation) => match context.lookup(value) {
                Some(Object::Dictionary(dict)) => Ok(Some((Some((*number, *generation)), dict.clone()))),
                None | Some(Object::Null) => Ok(None),
                _ => Err("invalid referenced PDF dictionary".into()),
            },
            Object::Dictionary(dict) => Ok(Some((None, dict.clone()))),
            _ => Ok(None),
        }
    }
    fn edit_array(context: &Context, value: Option<&Object>) -> Result<(Option<(f64, f64)>, Vec<Object>), String> {
        match value {
            Some(reference @ Object::Reference(number, generation)) => match context.lookup(reference) {
                Some(Object::Array(array)) => Ok((Some((*number, *generation)), array.clone())),
                None | Some(Object::Null) => Ok((None, Vec::new())),
                _ => Err("invalid referenced PDF array".into()),
            },
            Some(Object::Array(array)) => Ok((None, array.clone())),
            _ => Ok((None, Vec::new())),
        }
    }
    fn set(dict: &mut Vec<(Vec<u8>, Object)>, name: &[u8], value: Object) {
        if let Some((_, existing)) = dict.iter_mut().find(|(key, _)| key == name) {
            *existing = value;
        } else {
            dict.push((name.to_vec(), value));
        }
    }
    // JavaScript's Map is keyed by object number, not the full reference.
    let mut touched: HashMap<u64, (RefKey, Object)> = HashMap::new();
    let mut touch = |reference: (f64, f64), object: Object| {
        let key = RefKey::new(reference.0, reference.1);
        touched.insert(key.0, (key, object));
    };
    let page_object = Object::Reference(page.0, page.1);
    let (_, mut page_dict) = edit_dict(original, Some(&page_object))?.ok_or("missing editable page")?;
    let (annots_ref, mut annots) = edit_array(original, field(&page_dict, b"Annots"))?;
    annots.push(Object::Reference(widget.0, widget.1));
    if let Some(reference) = annots_ref { touch(reference, Object::Array(annots)); }
    else { set(&mut page_dict, b"Annots", Object::Array(annots)); }
    touch(page, Object::Dictionary(page_dict));

    let root = original.root.as_ref().ok_or("missing catalog reference")?;
    let Object::Reference(root_number, root_generation) = root else { return Err("invalid catalog reference".into()) };
    let (_, mut catalog) = edit_dict(original, Some(root))?.ok_or("missing editable catalog")?;
    let (acro_ref, mut acro) = edit_dict(original, field(&catalog, b"AcroForm"))?.unwrap_or((None, Vec::new()));
    let (fields_ref, mut fields) = edit_array(original, field(&acro, b"Fields"))?;
    fields.push(Object::Reference(widget.0, widget.1));
    if let Some(reference) = fields_ref { touch(reference, Object::Array(fields)); }
    else { set(&mut acro, b"Fields", Object::Array(fields)); }
    set(&mut acro, b"SigFlags", Object::Number(3.0));
    if let Some(reference) = acro_ref { touch(reference, Object::Dictionary(acro)); }
    else { set(&mut catalog, b"AcroForm", Object::Dictionary(acro)); }
    if certify {
        set(&mut catalog, b"Perms", dictionary([(b"DocMDP".as_slice(), Object::Reference(signature.0, signature.1))]));
    }
    if acro_ref.is_none() || certify { touch((*root_number, *root_generation), Object::Dictionary(catalog)); }

    for (key, expected) in touched.values() {
        let actual = revised.objects.get(key).ok_or("missing rewritten PDF object")?;
        if !equivalent(expected, actual) { return Err("rewritten PDF object differs from signing operation".into()); }
    }
    for key in &original.order {
        let before = &original.objects[key];
        let after = revised.objects.get(key).ok_or("missing original PDF object")?;
        if !equivalent(before, after) && !touched.values().any(|(target, _)| target == key) {
            return Err("unrelated original PDF object was modified".into());
        }
    }
    Ok(touched.into_values().map(|(key, _)| key).collect())
}

/// Compare parsed object graphs against the operation owned by native code.
/// This is one component of revision validation, not a signing authorization:
/// xref serialization, physical object occurrence and parser parity are
/// checked separately before any private-key operation can be permitted.
pub(super) fn validate_graph(
    original: &Context,
    revised: &Context,
    previous_size: f64,
    options: &SignOptions,
    signing_time: &str,
    range: [usize; 4],
) -> Result<HashSet<RefKey>, String> {
    let largest = original.largest_object_number.max(previous_size - 1.0);
    if !largest.is_finite() || largest < 0.0 || largest.fract() != 0.0 || largest > 9_007_199_254_740_980.0 {
        return Err("invalid next PDF object number".into());
    }
    let first = largest + 1.0;
    let signature = (first, 0.0);
    let widget = (first + 1.0, 0.0);
    let (count, field_name) = new_field_name(original)?;
    if options.certify_level.is_some() && count != 0 { return Err("cannot certify an already signed PDF".into()); }
    let page_index = options.visible.as_ref().map_or(0.0, |visible| visible.page_index);
    if !page_index.is_finite() || page_index < 0.0 || page_index.fract() != 0.0 || page_index > usize::MAX as f64 {
        return Err("invalid signature page index".into());
    }
    let page = *pages(original)?.get(page_index as usize).ok_or("missing signature page")?;
    let sig = revised.objects.get(&RefKey::new(signature.0, 0.0)).ok_or("missing signature object")?;
    signature_dictionary(sig, options, signing_time, range)?;
    let placed = options.visible.as_ref().map(|visible| placement(original, page, visible)).transpose()?;
    let widget_object = revised.objects.get(&RefKey::new(widget.0, 0.0)).ok_or("missing widget object")?;
    let appearance = widget_dictionary(widget_object, &field_name, signature, page, placed.as_ref())?;
    let mut physical = validate_existing_edits(original, revised, page, widget, signature, options.certify_level.is_some())?;
    let mut created = HashSet::from([RefKey::new(signature.0, 0.0), RefKey::new(widget.0, 0.0)]);
    if let (Some(visible), Some(placed), Some(appearance)) = (&options.visible, placed.as_ref(), appearance) {
        let image = appearance_forms(revised, appearance, placed)?;
        let mask = appearance_image(revised, image, &visible.png)?;
        let image_expected = first + 2.0;
        let paint = image_expected + if mask.is_some() { 2.0 } else { 1.0 };
        if image != (image_expected, 0.0)
            || mask.is_some_and(|reference| reference != (image_expected + 1.0, 0.0))
            || appearance != (paint + 3.0, 0.0)
        {
            return Err("appearance object allocation differs from native operation".into());
        }
        for number in image_expected as u64..=(paint + 3.0) as u64 {
            created.insert(RefKey::new(number as f64, 0.0));
        }
    }
    for key in revised.objects.keys() {
        if !original.objects.contains_key(key) && !created.contains(key) {
            return Err("unrelated new PDF object".into());
        }
    }
    if created.iter().any(|key| original.objects.contains_key(key) || !revised.objects.contains_key(key)) {
        return Err("missing or colliding new PDF object".into());
    }
    physical.extend(created);
    Ok(physical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::Parser;

    #[test]
    fn finds_selected_page_and_reports_redefined_original_object() {
        let original = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R >> endobj\ntrailer << /Root 1 0 R >>\nstartxref\n0\n%%EOF\n";
        let old = Parser::new(original).document().unwrap();
        assert_eq!(pages(&old).unwrap(), vec![(3.0, 0.0)]);
        let mut revision = original.to_vec();
        revision.extend_from_slice(b"3 0 obj << /Type /Page /Parent 2 0 R /Contents 9 0 R >> endobj\n");
        let new = Parser::new(&revision).document().unwrap();
        let delta = changes(&old, &new);
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].reference, (3.0, 0.0));
    }

    #[test]
    fn signature_metadata_is_bound_to_native_options() {
        let input = b"<< /Type /Sig /Filter /Adobe.PPKLite /SubFilter /adbe.pkcs7.detached /Reason <FEFFD55C> /M (D:20250101000000Z) /ByteRange [0 10 14 10] /Contents <00> >>";
        let actual = Parser::new(input).object().unwrap();
        let mut options = SignOptions::default();
        options.reason = Some("한".into());
        assert!(signature_dictionary(&actual, &options, "D:20250101000000Z", [0, 10, 14, 10]).is_ok());
        options.reason = Some("different".into());
        assert!(signature_dictionary(&actual, &options, "D:20250101000000Z", [0, 10, 14, 10]).is_err());
    }

    #[test]
    fn derives_rotated_placement_from_inherited_page_geometry() {
        let input = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 100] /Rotate 90 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R >> endobj\ntrailer << /Root 1 0 R >>\nstartxref\n0\n%%EOF\n";
        let context = Parser::new(input).document().unwrap();
        let visible = VisibleSignature { page_index: 0.0, position: "bottom-right".into(), width: 40.0, height: 20.0, png: Vec::new() };
        assert_eq!(placement(&context, (3.0, 0.0), &visible).unwrap(), Placement {
            rect: [170.0, 50.0, 190.0, 90.0], rotation: 90.0, width: 40.0, height: 20.0,
        });
    }

    #[test]
    fn only_native_page_and_form_mutations_are_accepted() {
        let original = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R /AcroForm 4 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /Annots 5 0 R >> endobj\n4 0 obj << /Fields 6 0 R /SigFlags 1 >> endobj\n5 0 obj [7 0 R] endobj\n6 0 obj [8 0 R] endobj\n7 0 obj << /Type /Annot >> endobj\n8 0 obj << /FT /Tx >> endobj\ntrailer << /Root 1 0 R >>\nstartxref\n1\n%%EOF\n";
        let old = Parser::new(original).document().unwrap();
        let mut revision = original.to_vec();
        revision.extend_from_slice(b"5 0 obj [7 0 R 10 0 R] endobj\n3 0 obj << /Type /Page /Parent 2 0 R /Annots 5 0 R >> endobj\n6 0 obj [8 0 R 10 0 R] endobj\n4 0 obj << /Fields 6 0 R /SigFlags 3 >> endobj\n");
        let updated = Parser::new(&revision).document().unwrap();
        assert!(validate_existing_edits(&old, &updated, (3.0, 0.0), (10.0, 0.0), (9.0, 0.0), false).is_ok());
        revision.extend_from_slice(b"7 0 obj << /Type /Annot /F 4 >> endobj\n");
        let altered = Parser::new(&revision).document().unwrap();
        assert!(validate_existing_edits(&old, &altered, (3.0, 0.0), (10.0, 0.0), (9.0, 0.0), false).is_err());
    }

    #[test]
    fn appearance_forms_reject_injected_operators() {
        fn form(resources: Object, bbox: [f64; 4], body: &[u8]) -> Object {
            Object::Stream(vec![
                (b"Type".to_vec(), Object::Name(b"XObject".to_vec())),
                (b"Subtype".to_vec(), Object::Name(b"Form".to_vec())),
                (b"BBox".to_vec(), Object::Array(bbox.into_iter().map(Object::Number).collect())),
                (b"Resources".to_vec(), resources),
                (b"Length".to_vec(), Object::Number(body.len() as f64)),
            ], body.to_vec())
        }
        fn resource(entries: &[(&[u8], f64)]) -> Object {
            Object::Dictionary(vec![(b"XObject".to_vec(), Object::Dictionary(entries.iter().map(|(name, number)| (name.to_vec(), Object::Reference(*number, 0.0))).collect()))])
        }
        let mut context = Context::default();
        let bbox = [0.0, 0.0, 40.0, 20.0];
        context.assign(1.0, 0.0, form(resource(&[(b"FRM", 2.0)]), bbox, b"/FRM Do"));
        context.assign(2.0, 0.0, form(resource(&[(b"n0", 3.0), (b"n2", 4.0)]), bbox, b"q 1 0 0 1 0 0 cm /n0 Do Q q 1 0 0 1 0 0 cm /n2 Do Q"));
        context.assign(3.0, 0.0, form(dictionary([]), [0.0, 0.0, 100.0, 100.0], b"% DSBlank"));
        context.assign(4.0, 0.0, form(resource(&[(b"Sig0", 5.0)]), bbox, b"q 40 0 0 20 0 0 cm /Sig0 Do Q"));
        let placed = Placement { rect: bbox, rotation: 0.0, width: 40.0, height: 20.0 };
        assert_eq!(appearance_forms(&context, (1.0, 0.0), &placed).unwrap(), (5.0, 0.0));
        context.assign(4.0, 0.0, form(resource(&[(b"Sig0", 5.0)]), bbox, b"q 40 0 0 20 0 0 cm /Sig0 Do Q BT"));
        assert!(appearance_forms(&context, (1.0, 0.0), &placed).is_err());
    }

    #[test]
    fn names_account_for_only_valued_signatures_and_utf16_names() {
        let input = b"%PDF-1.7\n1 0 obj << /Type /Catalog /AcroForm << /Fields [2 0 R 3 0 R 4 0 R] >> >> endobj\n2 0 obj << /FT /Sig /T <FEFF005300690067006E006100740075007200650033> /V << /ByteRange [0 1 2 3] >> >> endobj\n3 0 obj << /FT /Sig /T (Signature1) /V << /ByteRange [0 1 2 3] >> >> endobj\n4 0 obj << /FT /Sig /T (unsigned) >> endobj\ntrailer << /Root 1 0 R >>\nstartxref\n1\n%%EOF\n";
        let context = Parser::new(input).document().unwrap();
        assert_eq!(new_field_name(&context).unwrap(), (2, "Signature4".to_owned()));
    }

    #[test]
    fn image_pixels_are_bound_to_request_png() {
        use image::ImageEncoder;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[10, 20, 30, 255], 1, 1, image::ExtendedColorType::Rgba8).unwrap();
        let body = vec![0x78, 0x01, 0x01, 3, 0, 0xfc, 0xff, 10, 20, 30, 0, 0, 0, 0];
        let dict = vec![
            (b"Type".to_vec(), Object::Name(b"XObject".to_vec())),
            (b"Subtype".to_vec(), Object::Name(b"Image".to_vec())),
            (b"BitsPerComponent".to_vec(), Object::Number(8.0)),
            (b"Width".to_vec(), Object::Number(1.0)),
            (b"Height".to_vec(), Object::Number(1.0)),
            (b"ColorSpace".to_vec(), Object::Name(b"DeviceRGB".to_vec())),
            (b"Filter".to_vec(), Object::Name(b"FlateDecode".to_vec())),
            (b"Length".to_vec(), Object::Number(body.len() as f64)),
        ];
        let mut context = Context::default();
        context.assign(1.0, 0.0, Object::Stream(dict.clone(), body.clone()));
        assert_eq!(appearance_image(&context, (1.0, 0.0), &png).unwrap(), None);
        let mut changed = body;
        changed[7] ^= 1;
        context.assign(1.0, 0.0, Object::Stream(dict, changed));
        assert!(appearance_image(&context, (1.0, 0.0), &png).is_err());
    }

    #[test]
    fn graph_binds_new_objects_to_the_requested_operation() {
        let original = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R >> endobj\ntrailer << /Root 1 0 R >>\nstartxref\n1\n%%EOF\n";
        let old = Parser::new(original).document().unwrap();
        let mut revision = original.to_vec();
        revision.extend_from_slice(b"3 0 obj << /Type /Page /Parent 2 0 R /Annots [5 0 R] >> endobj\n1 0 obj << /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [5 0 R] /SigFlags 3 >> >> endobj\n5 0 obj << /Type /Annot /Subtype /Widget /FT /Sig /T (Signature1) /V 4 0 R /F 132 /Rect [0 0 0 0] /P 3 0 R >> endobj\n4 0 obj << /Type /Sig /Filter /Adobe.PPKLite /SubFilter /adbe.pkcs7.detached /M (D:20250101000000Z) /ByteRange [0 0 4 0] /Contents <00> >> endobj\n");
        let revised = Parser::new(&revision).document().unwrap();
        assert!(validate_graph(&old, &revised, 4.0, &SignOptions::default(), "D:20250101000000Z", [0, 0, 4, 0]).is_ok());
        revision.extend_from_slice(b"6 0 obj << /Type /Action /S /JavaScript >> endobj\n");
        let altered = Parser::new(&revision).document().unwrap();
        assert!(validate_graph(&old, &altered, 4.0, &SignOptions::default(), "D:20250101000000Z", [0, 0, 4, 0]).is_err());
    }
}
