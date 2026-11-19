mod flate;

use super::{Context, Object};

pub(in crate::host::d2::pdf) fn decode(
    context: &Context,
    dict: &[(Vec<u8>, Object)],
    input: &[u8],
) -> Result<Vec<u8>, String> {
    let filter = context.lookup_field(dict, b"Filter");
    let params = context.lookup_field(dict, b"DecodeParms");
    let mut data = input.to_vec();
    match filter {
        None => {}
        Some(Object::Name(name)) => data = apply(context, name, params, &data)?,
        Some(Object::Array(filters)) => {
            for (index, filter) in filters.iter().enumerate() {
                let Some(Object::Name(name)) = context.lookup(filter) else {
                    return Err("invalid PDF stream filter".into());
                };
                let item = match params {
                    None | Some(Object::Null) => None,
                    Some(Object::Array(values)) => {
                        values.get(index).and_then(|value| context.lookup(value))
                    }
                    _ => return Err("invalid PDF decode parameters".into()),
                };
                data = apply(context, name, item, &data)?;
            }
        }
        _ => return Err("invalid PDF stream filter".into()),
    }
    Ok(data)
}

fn apply(
    context: &Context,
    name: &[u8],
    params: Option<&Object>,
    data: &[u8],
) -> Result<Vec<u8>, String> {
    match name {
        b"FlateDecode" => flate::decode(data),
        b"ASCIIHexDecode" => Ok(ascii_hex(data)),
        b"ASCII85Decode" => Ok(ascii85(data)),
        b"RunLengthDecode" => Ok(run_length(data)),
        b"LZWDecode" => {
            let early = match params {
                Some(Object::Dictionary(dict)) => {
                    match context.lookup_field(dict, b"EarlyChange") {
                        Some(Object::Number(value)) => *value as i32,
                        _ => 1,
                    }
                }
                _ => 1,
            };
            Ok(lzw(data, early))
        }
        _ => Err("unsupported PDF stream encoding".into()),
    }
}

fn ascii_hex(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2);
    let mut first = None;
    for &byte in data {
        if byte == b'>' {
            if let Some(nibble) = first {
                out.push(nibble << 4);
            }
            break;
        }
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'A'..=b'F' => byte - b'A' + 10,
            b'a'..=b'f' => byte - b'a' + 10,
            _ => continue,
        };
        if let Some(nibble) = first.take() {
            out.push((nibble << 4) | digit);
        } else {
            first = Some(digit);
        }
    }
    out
}

fn ascii85(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut position = 0usize;
    let next = |position: &mut usize| -> Option<u8> {
        while data
            .get(*position)
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            *position += 1;
        }
        let value = data.get(*position).copied();
        if value.is_some() {
            *position += 1;
        }
        value
    };
    while let Some(first) = next(&mut position) {
        if first == b'~' {
            break;
        }
        if first == b'z' {
            out.extend_from_slice(&[0; 4]);
            continue;
        }
        let mut digits = [117u8; 5];
        digits[0] = first;
        let mut count = 1;
        while count < 5 {
            match next(&mut position) {
                Some(b'~') | None => break,
                Some(value) => {
                    digits[count] = value;
                    count += 1;
                }
            }
        }
        let mut value = 0i64;
        for digit in digits {
            value = value * 85 + i64::from(digit) - 33;
        }
        let bytes = (value as u32).to_be_bytes();
        out.extend_from_slice(&bytes[..count - 1]);
        if count < 5 {
            break;
        }
    }
    out
}

fn run_length(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 1 < data.len() {
        let count = data[pos];
        let first = data[pos + 1];
        pos += 2;
        if count == 128 {
            break;
        }
        if count < 128 {
            out.push(first);
            let wanted = usize::from(count);
            let available = wanted.min(data.len() - pos);
            out.extend_from_slice(&data[pos..pos + available]);
            out.resize(out.len() + wanted - available, 0);
            pos += available;
        } else {
            out.extend(std::iter::repeat_n(first, 257 - usize::from(count)));
        }
    }
    out
}

fn lzw(data: &[u8], early_change: i32) -> Vec<u8> {
    let mut values = [0u8; 4096];
    let mut lengths = [0u16; 4096];
    let mut previous = [0u16; 4096];
    for index in 0..256 {
        values[index] = index as u8;
        lengths[index] = 1;
    }
    let mut sequence = [0u8; 4096];
    let mut sequence_len = 0usize;
    let mut previous_code = 0usize;
    let mut next_code = 258usize;
    let mut code_length = 9u32;
    let mut cached = 0u32;
    let mut cached_bits = 0u32;
    let mut position = 0usize;
    let mut out = Vec::new();
    loop {
        let mut eof = false;
        for _ in 0..512 {
            while cached_bits < code_length {
                let Some(&byte) = data.get(position) else {
                    eof = true;
                    break;
                };
                position += 1;
                cached = cached.wrapping_shl(8) | u32::from(byte);
                cached_bits += 8;
            }
            let code = if eof {
                0
            } else {
                cached_bits -= code_length;
                ((cached >> cached_bits) & ((1 << code_length) - 1)) as usize
            };
            let has_previous = sequence_len > 0;
            if code < 256 {
                sequence[0] = code as u8;
                sequence_len = 1;
            } else if code >= 258 {
                if code < next_code {
                    sequence_len = usize::from(lengths.get(code).copied().unwrap_or(0));
                    let mut cursor = code;
                    for index in (0..sequence_len).rev() {
                        if index < sequence.len() {
                            sequence[index] = values.get(cursor).copied().unwrap_or(0);
                        }
                        cursor = usize::from(previous.get(cursor).copied().unwrap_or(0));
                    }
                } else {
                    if sequence_len < sequence.len() {
                        sequence[sequence_len] = sequence[0];
                    }
                    sequence_len += 1;
                }
            } else if code == 256 {
                code_length = 9;
                next_code = 258;
                sequence_len = 0;
                continue;
            } else {
                return out;
            }
            if has_previous {
                if next_code < 4096 {
                    previous[next_code] = previous_code as u16;
                    lengths[next_code] = lengths
                        .get(previous_code)
                        .copied()
                        .unwrap_or(0)
                        .wrapping_add(1);
                    values[next_code] = sequence[0];
                }
                next_code += 1;
                let boundary = (next_code as i64 + i64::from(early_change)) as u64;
                if boundary.is_power_of_two() {
                    code_length = (64 - boundary.leading_zeros()).min(12);
                }
            }
            previous_code = code;
            let copied = sequence_len.min(sequence.len());
            out.extend_from_slice(&sequence[..copied]);
            out.resize(out.len() + sequence_len - copied, 0);
        }
        if eof {
            return out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_ascii_and_run_length_sequences() {
        assert_eq!(ascii_hex(b"41 4>"), b"A@");
        assert_eq!(ascii85(b"z~>"), [0, 0, 0, 0]);
        assert_eq!(run_length(&[1, b'A', b'B', 254, b'C', 128, 0]), b"ABCCC");
    }
}
