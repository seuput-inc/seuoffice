//! Native stream recompression and lossless-dictionary image downsampling.
use super::document_edits::session::Session;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};
use serde_json::{Value, json};
use shell_contract::runtime::CancelToken;
use shell_ipc_d2::pdf::editor::hosts::Shrunk;
use std::collections::HashMap;
use std::io::Write;

const MAX_DIMENSION: f64 = 2480.0;
#[derive(Debug, Clone)]
struct Stream {
    reference: String,
    subtype: String,
    filter: String,
    width: f64,
    height: f64,
    bits: f64,
    color: Option<String>,
    decode_parms: bool,
    matte: bool,
    mask_present: bool,
    mask_ref: Option<String>,
    mask_is_stream: bool,
    #[cfg(test)]
    bytes: Vec<u8>,
}
#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    width: f64,
    height: f64,
    channels: usize,
}
impl Stream {
    fn from_value(v: &Value) -> Result<Self, String> {
        let string = |name: &str| {
            v.get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("invalid PDF shrink {name}"))
        };
        let num = |name: &str| {
            v.get(name)
                .and_then(Value::as_f64)
                .ok_or_else(|| format!("invalid PDF shrink {name}"))
        };
        let flag = |name: &str| {
            v.get(name)
                .and_then(Value::as_bool)
                .ok_or_else(|| format!("invalid PDF shrink {name}"))
        };
        Ok(Self {
            reference: string("ref")?,
            subtype: string("subtype")?,
            filter: string("filter")?,
            width: num("width")?,
            height: num("height")?,
            bits: num("bitsPerComponent")?,
            color: v
                .get("colorSpace")
                .and_then(Value::as_str)
                .map(str::to_owned),
            decode_parms: flag("decodeParms")?,
            matte: flag("matte")?,
            mask_present: flag("maskPresent")?,
            mask_ref: v.get("maskRef").and_then(Value::as_str).map(str::to_owned),
            mask_is_stream: flag("maskIsStream")?,
            #[cfg(test)]
            bytes: Vec::new(),
        })
    }
    fn candidate(&self) -> Option<Candidate> {
        if self.width.max(self.height) == 0.0
            || self.width.max(self.height) <= MAX_DIMENSION
            || self.bits != 8.0
            || self.filter != "/FlateDecode"
            || self.matte
        {
            return None;
        }
        let channels = match self.color.as_deref() {
            Some("/DeviceGray") => 1,
            Some("/DeviceRGB") => 3,
            Some("/DeviceCMYK") => 4,
            _ => return None,
        };
        Some(Candidate {
            width: self.width,
            height: self.height,
            channels,
        })
    }
}
#[derive(Debug)]
struct Replacement {
    reference: String,
    bytes: Vec<u8>,
    geometry: Option<(usize, usize)>,
}
#[derive(Debug, Default)]
struct Plan {
    replacements: Vec<Replacement>,
    streams: usize,
    images: usize,
}
fn inflate(bytes: &[u8], cancel: &CancelToken) -> Result<Option<Vec<u8>>, String> {
    let mut decoder = Decompress::new(true);
    let mut out = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        if cancel.is_cancelled() {
            return Err("PDF shrink cancelled".into());
        }
        let read = decoder.total_in();
        let written = decoder.total_out();
        let Ok(status) =
            decoder.decompress(&bytes[read as usize..], &mut chunk, FlushDecompress::Finish)
        else {
            return Ok(None);
        };
        let produced = (decoder.total_out() - written) as usize;
        if out.try_reserve(produced).is_err() {
            return Ok(None);
        }
        out.extend_from_slice(&chunk[..produced]);
        match status {
            Status::StreamEnd => return Ok(Some(out)),
            Status::Ok | Status::BufError if produced == 0 && decoder.total_in() == read => {
                return Ok(None);
            }
            _ => {}
        }
    }
}
fn deflate(bytes: &[u8], cancel: &CancelToken) -> Result<Vec<u8>, String> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    for chunk in bytes.chunks(65536) {
        if cancel.is_cancelled() {
            return Err("PDF shrink cancelled".into());
        }
        encoder.write_all(chunk).map_err(|e| e.to_string())?;
    }
    if cancel.is_cancelled() {
        return Err("PDF shrink cancelled".into());
    }
    encoder.finish().map_err(|e| e.to_string())
}
fn resample(
    pixels: &[u8],
    candidate: Candidate,
    new_width: usize,
    new_height: usize,
    cancel: &CancelToken,
) -> Result<Vec<u8>, String> {
    let len = new_width
        .checked_mul(new_height)
        .and_then(|v| v.checked_mul(candidate.channels))
        .ok_or("PDF image dimensions exceed address space")?;
    let mut out = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|e| format!("PDF image allocation failed: {e}"))?;
    out.resize(len, 0);
    for y in 0..new_height {
        if cancel.is_cancelled() {
            return Err("PDF shrink cancelled".into());
        }
        let sy = (candidate.height - 1.0)
            .min(((y as f64 + 0.5) * candidate.height / new_height as f64).floor());
        for x in 0..new_width {
            let sx = (candidate.width - 1.0)
                .min(((x as f64 + 0.5) * candidate.width / new_width as f64).floor());
            let src = (sy * candidate.width + sx) * candidate.channels as f64;
            let dst = (y * new_width + x) * candidate.channels;
            for c in 0..candidate.channels {
                let at = src + c as f64;
                if at >= 0.0 && at.is_finite() && at.fract() == 0.0 && at < usize::MAX as f64 {
                    out[dst + c] = pixels.get(at as usize).copied().unwrap_or(0);
                }
            }
        }
    }
    Ok(out)
}
fn plan(
    mut streams: Vec<Stream>,
    cancel: &CancelToken,
    mut read: impl FnMut(&Stream) -> Result<Vec<u8>, String>,
) -> Result<Plan, String> {
    let by_ref: HashMap<String, usize> = streams
        .iter()
        .enumerate()
        .map(|(i, s)| (s.reference.clone(), i))
        .collect();
    // The original map holds the pre-assignment base object, even after a stream is replaced.
    let mut bases: HashMap<String, Option<Candidate>> = HashMap::new();
    for s in &streams {
        if s.subtype == "/Image"
            && s.mask_is_stream
            && let Some(mask) = &s.mask_ref
        {
            bases.insert(mask.clone(), s.candidate());
        }
    }
    let mut result = Plan::default();
    for i in 0..streams.len() {
        if cancel.is_cancelled() {
            return Err("PDF shrink cancelled".into());
        }
        if streams[i].subtype == "/Image" {
            let Some(candidate) = streams[i].candidate() else {
                continue;
            };
            if streams[i].mask_present {
                let Some(other) = streams[i]
                    .mask_ref
                    .as_ref()
                    .and_then(|r| by_ref.get(r))
                    .map(|index| &streams[*index])
                else {
                    continue;
                };
                if !streams[i].mask_is_stream
                    || !other
                        .candidate()
                        .is_some_and(|c| c.width == candidate.width && c.height == candidate.height)
                {
                    continue;
                }
            }
            if let Some(base) = bases.get(&streams[i].reference)
                && !base.is_some_and(|c| c.width == candidate.width && c.height == candidate.height)
            {
                continue;
            }
            if streams[i].decode_parms {
                continue;
            }
            let bytes = read(&streams[i])?;
            let Some(decoded) = inflate(&bytes, cancel)? else {
                continue;
            };
            if (decoded.len() as f64)
                < candidate.width * candidate.height * candidate.channels as f64
            {
                continue;
            }
            let scale = MAX_DIMENSION / candidate.width.max(candidate.height);
            let w = (candidate.width * scale).floor().max(1.0) as usize;
            let h = (candidate.height * scale).floor().max(1.0) as usize;
            let pixels = resample(&decoded, candidate, w, h, cancel)?;
            let packed = deflate(&pixels, cancel)?;
            result.replacements.push(Replacement {
                reference: streams[i].reference.clone(),
                bytes: packed,
                geometry: Some((w, h)),
            });
            result.images += 1;
            streams[i].width = w as f64;
            streams[i].height = h as f64;
        } else if streams[i].filter == "/FlateDecode" {
            if streams[i].decode_parms {
                continue;
            }
            let bytes = read(&streams[i])?;
            let Some(decoded) = inflate(&bytes, cancel)? else {
                continue;
            };
            let packed = deflate(&decoded, cancel)?;
            if packed.len() >= bytes.len() {
                continue;
            }
            result.replacements.push(Replacement {
                reference: streams[i].reference.clone(),
                bytes: packed,
                geometry: None,
            });
            result.streams += 1;
        }
    }
    if cancel.is_cancelled() {
        return Err("PDF shrink cancelled".into());
    }
    Ok(result)
}
pub(super) fn shrink(source: &[u8], cancel: &CancelToken) -> Result<Shrunk, String> {
    let mut session = Session::open(source, cancel.clone())?;
    let value = session.exchange(json!({"op":"shrinkStreams","metadataOnly":true}))?;
    let streams = value
        .as_array()
        .ok_or("invalid PDF shrink stream list")?
        .iter()
        .map(Stream::from_value)
        .collect::<Result<Vec<_>, _>>()?;
    let plan = plan(streams, cancel, |stream| {
        let value = session.exchange(json!({"op":"readRawStream","ref":stream.reference}))?;
        let encoded = value.as_str().ok_or("invalid PDF stream bytes")?;
        STANDARD
            .decode(encoded)
            .map_err(|e| format!("invalid PDF stream bytes: {e}"))
    })?;
    let replacements: Vec<_> = plan
        .replacements
        .into_iter()
        .map(|r| {
            let mut value = json!({"ref":r.reference,"bytesBase64":STANDARD.encode(r.bytes)});
            if let Some((width, height)) = r.geometry {
                value["width"] = json!(width);
                value["height"] = json!(height);
            }
            value
        })
        .collect();
    session.exchange(json!({"op":"replaceRawStreams","replacements":replacements}))?;
    let result = session.exchange(json!({"op":"save","useObjectStreams":true}))?;
    let bytes = STANDARD
        .decode(result.as_str().ok_or("invalid PDF shrink output")?)
        .map_err(|e| format!("invalid PDF shrink output: {e}"))?;
    Ok(Shrunk {
        bytes,
        streams_recompressed: plan.streams as f64,
        images_downsampled: plan.images as f64,
    })
}

#[cfg(test)]
#[path = "shrink/oracle.rs"]
mod oracle;

#[cfg(test)]
mod tests {
    use super::*;
    fn stream(reference: &str, bytes: Vec<u8>) -> Stream {
        Stream {
            reference: reference.into(),
            subtype: String::new(),
            filter: "/FlateDecode".into(),
            width: 0.0,
            height: 0.0,
            bits: 8.0,
            color: None,
            decode_parms: false,
            matte: false,
            mask_present: false,
            mask_ref: None,
            mask_is_stream: false,
            bytes,
        }
    }
    fn encoded(bytes: &[u8], compression: Compression) -> Vec<u8> {
        let mut z = ZlibEncoder::new(Vec::new(), compression);
        z.write_all(bytes).unwrap();
        z.finish().unwrap()
    }
    fn run(streams: Vec<Stream>, cancel: &CancelToken) -> Result<Plan, String> {
        plan(streams, cancel, |s| Ok(s.bytes.clone()))
    }
    fn image(reference: &str, width: usize, height: usize) -> Stream {
        let mut s = stream(
            reference,
            encoded(&vec![127; width * height], Compression::fast()),
        );
        s.subtype = "/Image".into();
        s.width = width as f64;
        s.height = height as f64;
        s.color = Some("/DeviceGray".into());
        s
    }
    #[test]
    fn recompression_keeps_larger_or_invalid_streams_and_counts_smaller_ones() {
        let compressible = stream("1:0", encoded(&vec![42; 4096], Compression::none()));
        let incompressible = stream("2:0", encoded(b"short", Compression::best()));
        let invalid = stream("3:0", vec![1, 2, 3, 4]);
        let mut predicted = stream("4:0", encoded(&[9; 100], Compression::none()));
        predicted.decode_parms = true;
        let result = run(
            vec![compressible, incompressible, invalid, predicted],
            &CancelToken::default(),
        )
        .unwrap();
        assert_eq!((result.streams, result.images), (1, 0));
        assert_eq!(result.replacements[0].reference, "1:0");
        let replacement = stream("1:0", result.replacements[0].bytes.clone());
        assert_eq!(
            inflate(&replacement.bytes, &CancelToken::default())
                .unwrap()
                .unwrap(),
            vec![42; 4096]
        );
        let mut truncated = encoded(&vec![1; 512], Compression::best());
        truncated.pop();
        assert!(
            inflate(&truncated, &CancelToken::default())
                .unwrap()
                .is_none()
        );
        let cancelled = CancelToken::default();
        cancelled.cancel();
        assert!(run(vec![replacement], &cancelled).is_err());
    }
    #[test]
    fn image_resampling_preserves_channels_and_source_masks_order() {
        let candidate = Candidate {
            width: 4.0,
            height: 2.0,
            channels: 3,
        };
        let pixels: Vec<u8> = (0..24).collect();
        assert_eq!(
            resample(&pixels, candidate, 2, 1, &CancelToken::default()).unwrap(),
            vec![15, 16, 17, 21, 22, 23]
        );
        let mut base = image("1:0", 2481, 1);
        base.mask_present = true;
        base.mask_ref = Some("2:0".into());
        base.mask_is_stream = true;
        let mask = image("2:0", 2481, 1);
        let both = run(vec![base.clone(), mask.clone()], &CancelToken::default()).unwrap();
        assert_eq!((both.images, both.streams), (2, 0));
        assert!(
            both.replacements
                .iter()
                .all(|r| r.geometry == Some((2480, 1)))
        );
        let reversed = run(vec![mask, base], &CancelToken::default()).unwrap();
        assert_eq!((reversed.images, reversed.streams), (1, 0));
    }
    #[test]
    fn retains_images_with_incompatible_masks_predictors_and_colors() {
        let mut base = image("1:0", 2481, 1);
        base.mask_present = true;
        base.mask_ref = Some("2:0".into());
        base.mask_is_stream = true;
        let mut mask = image("2:0", 2481, 1);
        mask.matte = true;
        let mut predicted = image("3:0", 2481, 1);
        predicted.decode_parms = true;
        let mut indexed = image("4:0", 2481, 1);
        indexed.color = None;
        let result = run(
            vec![base, mask, predicted, indexed],
            &CancelToken::default(),
        )
        .unwrap();
        assert_eq!((result.images, result.streams), (0, 0));
        assert!(result.replacements.is_empty());
    }
    #[test]
    fn resamples_device_rgb_and_cmyk_but_not_short_pixel_data() {
        let mut rgb = image("1:0", 2481, 1);
        rgb.color = Some("/DeviceRGB".into());
        rgb.bytes = encoded(&vec![23; 2481 * 3], Compression::fast());
        let mut cmyk = image("2:0", 2481, 1);
        cmyk.color = Some("/DeviceCMYK".into());
        cmyk.bytes = encoded(&vec![17; 2481 * 4], Compression::fast());
        let mut short = image("3:0", 2481, 1);
        short.color = Some("/DeviceRGB".into());
        let result = run(vec![rgb, cmyk, short], &CancelToken::default()).unwrap();
        assert_eq!((result.images, result.streams), (2, 0));
        let cancel = CancelToken::default();
        assert_eq!(
            inflate(&result.replacements[0].bytes, &cancel)
                .unwrap()
                .unwrap()
                .len(),
            2480 * 3
        );
        assert_eq!(
            inflate(&result.replacements[1].bytes, &cancel)
                .unwrap()
                .unwrap()
                .len(),
            2480 * 4
        );
    }
}
