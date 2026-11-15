//! Native byte codecs of `ppt2pptx` and the ODP/OTP half of `odf2ooxml`.
//! Dialogs, settings, source authorization and atomic destination writes belong to the caller.

mod odf;
mod odp;
mod ppt;
mod ppt_writer;
mod styles;
mod xml;

pub const PPT_MAX_BYTES: usize = 256 * 1024 * 1024;
pub const PPT_MAX_STREAM_BYTES: usize = 128 * 1024 * 1024;

/// Conversion metadata matches the original byte codecs. The shell displays
/// legacy PPT warnings, not ODF's diagnostic drop counters as new dialogs.
#[derive(Debug)]
pub struct PresentationConversion {
    pub bytes: Vec<u8>,
    pub slides: usize,
    pub warnings: Vec<String>,
    pub dropped: Vec<(String, usize)>,
}
type Converted = PresentationConversion;

struct Stop<'a>(&'a dyn Fn() -> bool);
impl Stop<'_> {
    fn check(&self) -> Result<(), String> {
        if (self.0)() {
            Err("conversion cancelled".to_owned())
        } else {
            Ok(())
        }
    }
}

/// `.ppt` always has the original text-only disclosure; ODP drop counts are not
/// new warning dialogs. The original ingress displays only legacy PPT warnings.
pub fn convert_presentation(
    bytes: &[u8],
    legacy_ppt: bool,
) -> Result<(Vec<u8>, Vec<String>), String> {
    convert_presentation_cancellable(bytes, legacy_ppt, &|| false)
}

/// Checks cancellation between record, XML, frame, text and image operations.
/// Shared bounded ZIP/CFB validation and PPTX open/save calls are synchronous.
pub fn convert_presentation_cancellable(
    bytes: &[u8],
    legacy_ppt: bool,
    cancelled: &dyn Fn() -> bool,
) -> Result<(Vec<u8>, Vec<String>), String> {
    let converted = convert_detailed(bytes, legacy_ppt, cancelled)?;
    Ok((converted.bytes, converted.warnings))
}

pub fn convert_detailed(
    bytes: &[u8],
    legacy_ppt: bool,
    cancelled: &dyn Fn() -> bool,
) -> Result<PresentationConversion, String> {
    let stop = Stop(cancelled);
    stop.check()?;
    if legacy_ppt {
        ppt::convert(bytes, &stop).map_err(|error| error.to_string())
    } else {
        odp::convert(bytes, &stop)
    }
}

#[cfg(test)]
mod tests;
