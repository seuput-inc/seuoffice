use super::super::registry::{
    OpContext, SlideTarget, field, op_name, record, resolve_element, resolve_slide, string,
};
use super::{number, rect};
use base64::Engine;
use jscompat::JsNum;
use pptx_engine::insert::{self, NewPictureOptions};
use pptx_engine::media_insert::{
    self, MediaBytes, Model3dView, NewMediaKind, NewMediaOptions, NewModel3dOptions,
};
use pptx_engine::narration::{self, NarrationAudio};
use pptx_engine::types::{Model3dRotation, SlideElement};
use shell_contract::{BytesKind, IpcValue};
use shell_ipc_d1::slides::engine::TxnRecord;

fn bytes(value: Option<&IpcValue>, op: &str, name: &str) -> Result<Vec<u8>, String> {
    let error = || {
        format!(
            "op \"{op}\" needs \"{name}\": image/media bytes (base64 string, data URL, or Uint8Array)."
        )
    };
    match value {
        Some(IpcValue::Bytes(BytesKind::Uint8Array, b)) if !b.is_empty() => Ok(b.clone()),
        Some(IpcValue::String(s)) if !s.is_empty() => {
            let b64 = if s.starts_with("data:") {
                s.split_once(',').map_or(s.as_str(), |(_, v)| v)
            } else {
                s.as_str()
            };
            let clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
            let unpadded = clean.trim_end_matches('=');
            if unpadded.is_empty()
                || clean.len() - unpadded.len() > 2
                || !unpadded
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
            {
                return Err(error());
            }
            let decoded = base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(unpadded)
                .map_err(|_| error())?;
            if decoded.is_empty() {
                Err(error())
            } else {
                Ok(decoded)
            }
        }
        _ => Err(error()),
    }
}
fn ext(value: Option<&IpcValue>, supplied: Option<&IpcValue>) -> Option<String> {
    if let Some(s) = supplied.and_then(string).filter(|s| !s.is_empty()) {
        return Some(s.into());
    }
    let s = value.and_then(string)?.strip_prefix("data:")?;
    let mime = s.split([';', ',']).next()?;
    Some(
        match mime {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/bmp" => "bmp",
            "image/webp" => "webp",
            "image/tiff" => "tif",
            "video/mp4" => "mp4",
            "video/quicktime" => "mov",
            "video/webm" => "webm",
            "audio/mpeg" => "mp3",
            "audio/wav" => "wav",
            "audio/mp4" => "m4a",
            _ => return None,
        }
        .into(),
    )
}
fn explicit_ext(op: &IpcValue) -> Option<String> {
    field(op, "ext")
        .and_then(string)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
fn poster(op: &IpcValue) -> Result<Option<MediaBytes>, String> {
    let Some(p) = field(op, "poster").filter(|p| {
        !matches!(
            p,
            IpcValue::Null | IpcValue::Undefined | IpcValue::Bool(false)
        )
    }) else {
        return Ok(None);
    };
    let data = field(p, "bytes");
    let extension = ext(data, field(p, "ext"));
    let data = bytes(data, "addMedia", "poster.bytes")?;
    let extension = extension
        .ok_or_else(|| "op \"addMedia\" needs \"poster.ext\" when a poster is given.".to_owned())?;
    Ok(Some(MediaBytes {
        bytes: data,
        ext: extension,
    }))
}
fn known(name: &str) -> bool {
    matches!(
        name,
        "addPicture"
            | "replacePicture"
            | "addMedia"
            | "addNarration"
            | "clearNarration"
            | "addModel3d"
            | "setModel3dView"
    )
}
fn validate_inner(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<(), String> {
    let name = op_name(op);
    if matches!(name, "replacePicture" | "setModel3dView") {
        let (target, index) = resolve_element(ctx, op, false)?;
        if name == "setModel3dView" {
            let el = &ctx
                .slide(&target)
                .elements
                .as_ref()
                .expect("resolved elements")[index];
            if !matches!(el, SlideElement::Picture(p) if p.model3d.is_some()) {
                return Err(format!(
                    "op \"setModel3dView\": element \"{}\" is not a 3D model.",
                    el.id()
                ));
            }
            for axis in ["ax", "ay", "az"] {
                if !number(field(op, axis)).is_some_and(f64::is_finite) {
                    return Err(format!(
                        "op \"setModel3dView\": \"{axis}\" must be a finite number."
                    ));
                }
            }
            if let Some(p) = field(op, "poster").filter(|v| !matches!(v, IpcValue::Undefined)) {
                bytes(Some(p), name, "poster")?;
                if field(op, "posterExt")
                    .and_then(string)
                    .is_none_or(str::is_empty)
                {
                    return Err("op \"setModel3dView\" needs \"posterExt\" with \"poster\".".into());
                }
            }
            return Ok(());
        }
    } else {
        resolve_slide(ctx, op, false)?;
    }
    if name == "clearNarration" {
        return Ok(());
    }
    if matches!(name, "addPicture" | "addMedia" | "addModel3d") {
        rect(op)?;
    }
    bytes(field(op, "bytes"), name, "bytes")?;
    let extension = if matches!(name, "addPicture" | "replacePicture" | "addMedia") {
        ext(field(op, "bytes"), field(op, "ext"))
    } else {
        explicit_ext(op)
    };
    if name == "addMedia" && !matches!(field(op, "kind").and_then(string), Some("audio" | "video"))
    {
        return Err("op \"addMedia\" needs \"kind\": \"video\" or \"audio\".".into());
    }
    if extension.is_none() {
        return Err(match name {
            "addPicture" => {
                "op \"addPicture\" needs \"ext\": the image extension (png/jpg/…).".into()
            }
            "replacePicture" => "op \"replacePicture\" needs \"ext\": the image extension.".into(),
            _ => format!("op \"{name}\" needs \"ext\"."),
        });
    }
    if name == "addMedia" {
        poster(op)?;
    }
    if name == "addNarration" && !number(field(op, "durationMs")).is_some_and(f64::is_finite) {
        return Err("op \"addNarration\": \"durationMs\" must be a finite number.".into());
    }
    Ok(())
}
pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    if known(op_name(op)) {
        Some(validate_inner(op, ctx))
    } else {
        None
    }
}
pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    if !known(op_name(op)) {
        return None;
    }
    Some((|| {
        validate_inner(op, ctx)?;
        let name = op_name(op);
        let mut result = record(op);
        let (target, id) = if matches!(name, "replacePicture" | "setModel3dView") {
            let (target, at) = resolve_element(ctx, op, false)?;
            let id = ctx
                .slide(&target)
                .elements
                .as_ref()
                .expect("resolved elements")[at]
                .id()
                .to_owned();
            (target, id)
        } else {
            (resolve_slide(ctx, op, false)?, String::new())
        };
        let SlideTarget::Slide(index) = target else {
            unreachable!()
        };
        if name == "clearNarration" {
            narration::clear_narration(ctx.opened, index)?;
            return Ok(result);
        }
        let uuid = jscompat::random_uuid();
        if name == "setModel3dView" {
            let poster = field(op, "poster")
                .filter(|v| !matches!(v, IpcValue::Undefined))
                .map(|v| {
                    Ok(MediaBytes {
                        bytes: bytes(Some(v), name, "poster")?,
                        ext: field(op, "posterExt").and_then(string).unwrap_or("").into(),
                    })
                })
                .transpose()?;
            let view = Model3dView {
                rot: Model3dRotation {
                    ax: JsNum(number(field(op, "ax")).unwrap_or_default()),
                    ay: JsNum(number(field(op, "ay")).unwrap_or_default()),
                    az: JsNum(number(field(op, "az")).unwrap_or_default()),
                },
                poster,
            };
            if !media_insert::set_model3d_view(ctx.opened, index, &id, &view)? {
                return Err(format!(
                    "op \"setModel3dView\": element \"{id}\" could not be turned."
                ));
            }
            return Ok(result);
        }
        let data = bytes(field(op, "bytes"), name, "bytes")?;
        let extension = if matches!(name, "addPicture" | "replacePicture" | "addMedia") {
            ext(field(op, "bytes"), field(op, "ext"))
        } else {
            explicit_ext(op)
        }
        .unwrap_or_default();
        let label = field(op, "name")
            .and_then(string)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let created = match name {
            "addPicture" => {
                let opts = NewPictureOptions {
                    bytes: data,
                    ext: extension.clone(),
                    offset: rect(op)?,
                    name: label,
                    descr: field(op, "descr")
                        .and_then(string)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned),
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| e.to_string())?
                    .as_millis() as f64;
                let picture = insert::add_picture(ctx.opened, index, &opts, &uuid, now)?.ok_or_else(|| format!("op \"addPicture\": unsupported image format \".{extension}\" (png/jpg/jpeg/gif/bmp/webp/tif supported)."))?;
                Some(
                    picture
                        .id
                        .as_deref()
                        .expect("inserted picture has an id")
                        .to_owned(),
                )
            }
            "replacePicture" => {
                if !pptx_engine::deck::shape::replace_picture_bytes(
                    ctx.opened,
                    index,
                    &id,
                    &data,
                    &extension,
                    matches!(field(op, "keepSrcRect"), Some(IpcValue::Bool(true))),
                )? {
                    return Err(format!(
                        "op \"replacePicture\": element \"{id}\" is not a picture or the format \".{extension}\" is unsupported."
                    ));
                }
                None
            }
            "addMedia" => {
                let opts = NewMediaOptions {
                    kind: if field(op, "kind").and_then(string) == Some("video") {
                        NewMediaKind::Video
                    } else {
                        NewMediaKind::Audio
                    },
                    bytes: data,
                    ext: extension,
                    poster: poster(op)?,
                    offset: rect(op)?,
                    name: label,
                };
                Some(
                    media_insert::add_media(ctx.opened, index, &opts, &uuid)?.ok_or_else(|| {
                        "op \"addMedia\": the media could not be embedded.".to_owned()
                    })?,
                )
            }
            "addNarration" => {
                let audio = NarrationAudio {
                    bytes: data,
                    ext: extension,
                    duration_ms: JsNum(number(field(op, "durationMs")).unwrap_or_default()),
                };
                let id = narration::add_narration(ctx.opened, index, &audio, &uuid)?.ok_or_else(
                    || "op \"addNarration\": the narration could not be embedded.".to_owned(),
                )?;
                narration::set_show_narration(ctx.opened)?;
                Some(id)
            }
            "addModel3d" => {
                let model_poster: Option<MediaBytes> = field(op, "poster")
                    .filter(|v| {
                        !matches!(
                            v,
                            IpcValue::Undefined | IpcValue::Null | IpcValue::Bool(false)
                        )
                    })
                    .map(super::ipc_de::decode)
                    .transpose()?;
                let opts = NewModel3dOptions {
                    bytes: data,
                    ext: extension,
                    poster: model_poster,
                    offset: rect(op)?,
                    name: label,
                    rot: None,
                };
                Some(
                    media_insert::add_model3d(ctx.opened, index, &opts, &uuid)?.ok_or_else(
                        || "op \"addModel3d\": the model could not be embedded.".to_owned(),
                    )?,
                )
            }
            _ => unreachable!(),
        };
        result.created = created.map(|id| vec![IpcValue::String(id)]);
        Ok(result)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn media_data_urls_decode_and_hint_extension() {
        let input = IpcValue::String("data:image/png;base64, aGk= ".into());
        assert_eq!(bytes(Some(&input), "addPicture", "bytes").unwrap(), b"hi");
        assert_eq!(ext(Some(&input), None).as_deref(), Some("png"));
        assert_eq!(
            ext(
                Some(&IpcValue::String("data:image/svg+xml;base64,aGk=".into())),
                None
            ),
            None
        );
        assert!(
            bytes(Some(&IpcValue::String("!!!!".into())), "addMedia", "bytes")
                .unwrap_err()
                .contains("bytes")
        );
    }
    #[test]
    fn image_data_url_lands_and_unsupported_image_reports_format() {
        let bytes = pptx_engine::blank::create_blank_pptx(0.0).unwrap();
        let mut opened = pptx_engine::deck::open_pptx(&bytes).unwrap();
        let mut ctx = OpContext {
            opened: &mut opened,
            parts: Default::default(),
            touched_parts: Default::default(),
            withhold: false,
        };
        let input = IpcValue::Object(vec![
            ("op".into(), IpcValue::String("addPicture".into())),
            (
                "target".into(),
                IpcValue::Object(vec![("slide".into(), IpcValue::Number(0.0))]),
            ),
            (
                "offset".into(),
                IpcValue::Object(vec![
                    ("x".into(), IpcValue::Number(0.0)),
                    ("y".into(), IpcValue::Number(0.0)),
                    ("cx".into(), IpcValue::Number(100.0)),
                    ("cy".into(), IpcValue::Number(100.0)),
                ]),
            ),
            (
                "bytes".into(),
                IpcValue::String("data:image/png;base64,aGk=".into()),
            ),
        ]);
        validate(&input, &mut ctx).unwrap().unwrap();
        let record = apply(&input, &mut ctx).unwrap().unwrap();
        assert_eq!(record.created.unwrap().len(), 1);
        let mut unsupported = input.clone();
        if let IpcValue::Object(fields) = &mut unsupported {
            fields.push(("ext".into(), IpcValue::String("svg".into())));
        }
        let error = apply(&unsupported, &mut ctx).unwrap().unwrap_err();
        assert!(error.contains("unsupported image format \".svg\""));
    }
}
