use super::{Context, Object};

fn string_bytes(object: &Object) -> Option<Vec<u8>> {
    match object {
        Object::HexString(raw) => Some(crate::host::d2::pdf::pdf_codec::hex_bytes(raw)),
        Object::String(raw) => Some(crate::host::d2::pdf::pdf_codec::literal_bytes(raw)),
        _ => None,
    }
}

pub(super) fn trailer_fields(original: &Context, previous_offset: usize) -> Result<String, String> {
    let Some(Object::Reference(root, generation)) = &original.root else {
        return Err("missing catalog reference".into());
    };
    let mut fields = format!(
        " /Root {} {} R",
        jscompat::number_to_string(*root),
        jscompat::number_to_string(*generation)
    );
    if let Some(Object::Reference(info, generation)) = &original.info {
        fields.push_str(&format!(
            " /Info {} {} R",
            jscompat::number_to_string(*info),
            jscompat::number_to_string(*generation)
        ));
    }
    if let Some(Object::Array(ids)) = &original.id {
        let converted: Option<Vec<Vec<u8>>> = ids.iter().map(string_bytes).collect();
        if let Some(converted) = converted.filter(|ids| !ids.is_empty()) {
            fields.push_str(" /ID [ ");
            for (index, id) in converted.iter().enumerate() {
                if index != 0 {
                    fields.push(' ');
                }
                fields.push('<');
                for byte in id {
                    fields.push_str(&format!("{byte:02X}"));
                }
                fields.push('>');
            }
            fields.push_str(" ]");
        }
    }
    fields.push_str(&format!(" /Prev {previous_offset}"));
    Ok(fields)
}
