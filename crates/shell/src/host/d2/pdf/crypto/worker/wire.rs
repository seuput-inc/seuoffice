//! Framing for one private document-signing worker, never a digest RPC.
use std::io::{Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Kind {
    Request = 1,
    Guard = 2,
    AdmitFirst = 3,
    AdmitRetry = 4,
    Permit = 5,
    Deny = 6,
    Signed = 7,
    Error = 8,
    Opened = 9,
    Verified = 10,
    OpenMessage = 11,
    ConfirmToken = 12,
    ConfirmAccept = 13,
    ConfirmDeny = 14,
}

impl Kind {
    fn from_byte(byte: u8) -> Result<Self, String> {
        match byte {
            1 => Ok(Self::Request), 2 => Ok(Self::Guard), 3 => Ok(Self::AdmitFirst),
            4 => Ok(Self::AdmitRetry), 5 => Ok(Self::Permit), 6 => Ok(Self::Deny),
            7 => Ok(Self::Signed), 8 => Ok(Self::Error), 9 => Ok(Self::Opened), 10 => Ok(Self::Verified),
            11 => Ok(Self::OpenMessage),
            12 => Ok(Self::ConfirmToken), 13 => Ok(Self::ConfirmAccept), 14 => Ok(Self::ConfirmDeny),
            _ => Err("invalid signing worker frame kind".into()),
        }
    }
}

/// The caller chooses the cap from the authorized request or expected result.
pub(super) fn read_frame(input: &mut impl Read, limit: usize) -> Result<(Kind, Vec<u8>), String> {
    let (kind, length) = read_header(input)?;
    read_body(input, kind, length, limit)
}

pub(super) fn read_header(input: &mut impl Read) -> Result<(Kind, usize), String> {
    let mut header = [0u8; 9];
    input.read_exact(&mut header).map_err(|error| format!("reading signing frame header: {error}"))?;
    let kind = Kind::from_byte(header[0])?;
    let length = u64::from_be_bytes(header[1..].try_into().map_err(|_| "invalid signing frame header")?);
    let length = usize::try_from(length).map_err(|_| "signing frame length overflow")?;
    Ok((kind, length))
}

pub(super) fn read_body(input: &mut impl Read, kind: Kind, length: usize, limit: usize) -> Result<(Kind, Vec<u8>), String> {
    if length > limit { return Err("signing frame exceeds request bound".into()); }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(|_| "signing frame allocation failed")?;
    bytes.resize(length, 0);
    input.read_exact(&mut bytes).map_err(|error| format!("reading signing frame body: {error}"))?;
    Ok((kind, bytes))
}

/// Allocate only bytes delivered by the private worker, not its declared total.
/// Provider chains and decoded document contents have no source-derived cap.
pub(super) fn read_streamed_frame(input: &mut impl Read) -> Result<(Kind, Vec<u8>), String> {
    let (kind, length) = read_header(input)?;
    let mut remaining = length;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 32768];
    while remaining != 0 {
        let take = remaining.min(chunk.len());
        input.read_exact(&mut chunk[..take]).map_err(|error| format!("reading signing frame body: {error}"))?;
        bytes.len().checked_add(take).ok_or("signing frame length overflow")?;
        bytes.try_reserve(take).map_err(|_| "signing frame allocation failed")?;
        bytes.extend_from_slice(&chunk[..take]);
        remaining -= take;
    }
    Ok((kind, bytes))
}

pub(super) fn write_frame(output: &mut impl Write, kind: Kind, bytes: &[u8], limit: usize) -> Result<(), String> {
    if bytes.len() > limit { return Err("signing frame exceeds request bound".into()); }
    let length = u64::try_from(bytes.len()).map_err(|_| "signing frame length overflow")?;
    output.write_all(&[kind as u8]).and_then(|_| output.write_all(&length.to_be_bytes()))
        .and_then(|_| output.write_all(bytes)).and_then(|_| output.flush())
        .map_err(|error| format!("writing signing frame: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_frames_round_trip_and_reject_truncation() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, Kind::AdmitRetry, b"synthetic evidence", 18).unwrap();
        let (kind, body) = read_frame(&mut bytes.as_slice(), 18).unwrap();
        assert_eq!(kind, Kind::AdmitRetry);
        assert_eq!(body, b"synthetic evidence");
        assert!(read_frame(&mut bytes.as_slice(), 3).is_err());
        assert!(read_frame(&mut &bytes[..bytes.len()-1], 18).is_err());
        bytes[0] = 255;
        assert!(read_frame(&mut bytes.as_slice(), 18).is_err());
    }
    #[test]
    fn streaming_reads_actual_chunks_and_rejects_missing_body() {
        let payload = vec![42; 32769];
        let mut pipe = Vec::new();
        write_frame(&mut pipe, Kind::Signed, &payload, payload.len()).unwrap();
        assert_eq!(read_streamed_frame(&mut pipe.as_slice()).unwrap(), (Kind::Signed, payload));
        assert!(read_streamed_frame(&mut &pipe[..pipe.len() - 1]).is_err());
        let mut forged = vec![Kind::Signed as u8];
        forged.extend_from_slice(&u64::MAX.to_be_bytes());
        assert!(read_streamed_frame(&mut forged.as_slice()).is_err());
    }
}
