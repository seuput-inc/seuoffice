use shell_contract::IpcValue;

fn length_key(key: &str, parent: Option<&str>) -> bool {
    key.ends_with("Emu") || matches!(key, "x" | "y" | "cx" | "cy" | "dx" | "dy")
        || (matches!(key, "l" | "t" | "r" | "b") && matches!(parent, Some("insets" | "insetsEmu")))
}

fn parse_length(value: &str) -> Option<f64> {
    let value = value.trim();
    let split = value.find(|c: char| c.is_ascii_alphabetic())?;
    let number = value[..split].trim();
    let digits = number.strip_prefix('-').unwrap_or(number);
    let mut parts = digits.split('.');
    if parts.next()?.is_empty() || !digits.chars().all(|c| c.is_ascii_digit() || c == '.') || parts.clone().count() > 1 || digits.ends_with('.') { return None; }
    let multiplier = match value[split..].to_ascii_lowercase().as_str() {
        "emu" => 1.0, "in" => 914400.0, "cm" => 360000.0, "mm" => 36000.0,
        "pt" => 12700.0, "pc" => 152400.0, "px" => 9525.0, _ => return None,
    };
    let result = number.parse::<f64>().ok()? * multiplier;
    result.is_finite().then(|| (result + 0.5).floor())
}

pub(super) fn normalize(value: &mut IpcValue, key: Option<&str>, parent: Option<&str>) {
    match value {
        IpcValue::String(text) if key.is_some_and(|k| length_key(k, parent)) => {
            if let Some(number) = parse_length(text) { *value = IpcValue::Number(number); }
        }
        IpcValue::Array(values) => for item in values { normalize(item, key, parent); },
        IpcValue::Object(fields) => for (name, item) in fields { normalize(item, Some(name), key); },
        _ => {}
    }
}

pub(super) fn normalize_media(op: &mut IpcValue) -> Result<(), String> {
    use shell_contract::BytesKind;

    fn normalize_source(value: &mut IpcValue, key: &str, infer_ext: bool) -> Result<(), String> {
        let IpcValue::Object(fields) = value else { return Ok(()); };
        let Some(bytes_at) = fields.iter().position(|(name, _)| name == key) else { return Ok(()); };
        let IpcValue::String(encoded) = &fields[bytes_at].1 else { return Ok(()); };
        let ext = encoded.strip_prefix("data:").and_then(|data| {
            let mime = data.split([';', ',']).next()?;
            Some(match mime {
                "image/png" => "png", "image/jpeg" => "jpg", "image/gif" => "gif",
                "image/bmp" => "bmp", "image/webp" => "webp", "image/tiff" => "tif",
                "video/mp4" => "mp4", "video/quicktime" => "mov", "video/webm" => "webm",
                "audio/mpeg" => "mp3", "audio/wav" => "wav", "audio/mp4" => "m4a",
                _ => return None,
            })
        });
        let base64 = if encoded.starts_with("data:") {
            encoded.split_once(',').map_or(encoded.as_str(), |(_, data)| data)
        } else { encoded.as_str() };
        let base64: String = base64.chars().filter(|c| !c.is_whitespace()).collect();
        let alphabet = base64.trim_end_matches('=');
        if alphabet.is_empty() || base64.len() - alphabet.len() > 2
            || !alphabet.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/') {
            return Err("invalid base64 media bytes".to_owned());
        }
        let decoded = jscompat::buffer_from_base64(&base64);
        if decoded.is_empty() { return Err("invalid base64 media bytes".to_owned()); }
        fields[bytes_at].1 = IpcValue::Bytes(BytesKind::Uint8Array, decoded);
        if let Some(ext) = ext.filter(|_| infer_ext) {
            match fields.iter_mut().find(|(key, _)| key == "ext") {
                Some((_, value)) if !matches!(&*value, IpcValue::String(s) if !s.is_empty()) => *value = IpcValue::String(ext.into()),
                None => fields.push(("ext".into(), IpcValue::String(ext.into()))),
                _ => {}
            }
        }
        Ok(())
    }

    let name = super::registry::op_name(op).to_owned();
    if matches!(name.as_str(), "addPicture" | "replacePicture" | "addMedia" | "addNarration" | "addModel3d") {
        normalize_source(op, "bytes", matches!(name.as_str(), "addPicture" | "replacePicture" | "addMedia"))?;
        if name == "addMedia" {
            if let IpcValue::Object(fields) = op {
                if let Some((_, poster)) = fields.iter_mut().find(|(key, _)| key == "poster") {
                    normalize_source(poster, "bytes", true)?;
                }
            }
        }
    } else if name == "setModel3dView" {
        normalize_source(op, "poster", false)?;
    } else if matches!(name.as_str(), "setBackground" | "setImageFill") {
        if let IpcValue::Object(fields) = op {
            if let Some((_, source)) = fields.iter_mut().find(|(key, _)| key == "source") {
                normalize_source(source, "bytes", true)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_nested_lengths_without_touching_other_values() {
        let mut op = IpcValue::Object(vec![
            ("box".into(), IpcValue::Object(vec![("x".into(), IpcValue::String("2.54cm".into()))])),
            ("colWidthsEmu".into(), IpcValue::Array(vec![IpcValue::String("1in".into())])),
            ("label".into(), IpcValue::String("1in".into())),
            ("missing".into(), IpcValue::Undefined),
        ]);
        normalize(&mut op, None, None);
        assert_eq!(op, IpcValue::Object(vec![
            ("box".into(), IpcValue::Object(vec![("x".into(), IpcValue::Number(914400.0))])),
            ("colWidthsEmu".into(), IpcValue::Array(vec![IpcValue::Number(914400.0)])),
            ("label".into(), IpcValue::String("1in".into())),
            ("missing".into(), IpcValue::Undefined),
        ]));
    }

    #[test]
    fn media_data_url_infers_extension_and_preserves_bytes() {
        let mut op = IpcValue::Object(vec![
            ("op".into(), IpcValue::String("addPicture".into())),
            ("bytes".into(), IpcValue::String("data:image/png;base64,AQID".into())),
        ]);
        normalize_media(&mut op).unwrap();
        assert_eq!(op, IpcValue::Object(vec![
            ("op".into(), IpcValue::String("addPicture".into())),
            ("bytes".into(), IpcValue::Bytes(shell_contract::BytesKind::Uint8Array, vec![1, 2, 3])),
            ("ext".into(), IpcValue::String("png".into())),
        ]));
    }
}
