// Derived from @pdf-lib/upng 1.0.1 UPNG.js, Copyright (c) 2017 Photopea.
// MIT license and complete notice are included in the parent upng.rs.
const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
const LENGTH_BASE: [usize; 32] = [3,4,5,6,7,8,9,10,11,13,15,17,19,23,27,31,35,43,51,59,67,83,99,115,131,163,195,227,258,999,999,999];
const LENGTH_EXTRA: [usize; 32] = [0,0,0,0,0,0,0,0,1,1,1,1,2,2,2,2,3,3,3,3,4,4,4,4,5,5,5,5,0,0,0,0];
const DIST_BASE: [usize; 32] = [1,2,3,4,5,7,9,13,17,25,33,49,65,97,129,193,257,385,513,769,1025,1537,2049,3073,4097,6145,8193,12289,16385,24577,65535,65535];
const DIST_EXTRA: [usize; 32] = [0,0,0,0,1,1,2,2,3,3,4,4,5,5,6,6,7,7,8,8,9,9,10,10,11,11,12,12,13,13,0,0];

struct Table { entries: Vec<u16>, bits: usize }

fn table(lengths: &[u8]) -> Table {
    let bits = lengths.iter().copied().max().unwrap_or(0) as usize;
    let mut entries = vec![0; 1usize << bits];
    let mut code = 0usize;
    for width in 1..=bits {
        for (symbol, &length) in lengths.iter().enumerate() {
            if usize::from(length) != width { continue; }
            let mut value = code;
            let mut reverse = 0;
            for _ in 0..width { reverse = (reverse << 1) | (value & 1); value >>= 1; }
            let mut slot = reverse;
            while slot < entries.len() {
                entries[slot] = ((symbol << 4) | width) as u16;
                slot += 1 << width;
            }
            code += 1;
        }
        code <<= 1;
    }
    Table { entries, bits }
}

struct Reader<'a> { data: &'a [u8], bit: usize }
impl Reader<'_> {
    fn peek(&self, width: usize) -> usize {
        let at = self.bit >> 3;
        let mut value = 0u64;
        for i in 0..4 { value |= u64::from(self.data.get(at + i).copied().unwrap_or(0)) << (8 * i); }
        ((value >> (self.bit & 7)) & ((1u64 << width) - 1)) as usize
    }
    fn take(&mut self, width: usize) -> usize {
        let value = self.peek(width);
        self.bit += width;
        value
    }
    fn symbol(&mut self, table: &Table) -> Result<usize, String> {
        let entry = table.entries[self.peek(table.bits)] as usize;
        let width = entry & 15;
        if width == 0 { return Err("UPNG inflater stalled on an invalid Huffman code".into()); }
        self.bit += width;
        Ok(entry >> 4)
    }
}

/// UPNG.inflateRaw with its caller-provided fixed-size output buffer. `input`
/// includes the two skipped zlib bytes and the four trailing bytes because a
/// stored-block typed-array view can address the original backing buffer.
pub(super) fn decode(input: &[u8], capacity: usize) -> Result<Vec<u8>, String> {
    let end = input.len().checked_sub(4).ok_or("truncated PNG deflate stream")?;
    let raw = input.get(2..end).ok_or("truncated PNG deflate stream")?;
    let mut output = Vec::new();
    output.try_reserve_exact(capacity).map_err(|_| "PNG allocation failed")?;
    output.resize(capacity, 0);
    if raw.starts_with(&[3, 0]) { return Ok(output); }
    let mut reader = Reader { data: raw, bit: 0 };
    let mut written = 0usize;
    let mut previous: Option<(Table, Table)> = None;
    loop {
        let final_block = reader.take(1) != 0;
        let kind = reader.take(2);
        if kind == 0 {
            reader.bit = (reader.bit + 7) & !7;
            let offset = reader.bit / 8;
            let length = usize::from(raw.get(offset).copied().unwrap_or(0)) | (usize::from(raw.get(offset + 1).copied().unwrap_or(0)) << 8);
            let start = offset.checked_add(6).ok_or("PNG inflate overflow")?;
            let end = start.checked_add(length).ok_or("PNG inflate overflow")?;
            let source = input.get(start..end).ok_or("invalid stored PNG block")?;
            let output_end = written.checked_add(length).ok_or("PNG inflate overflow")?;
            let target = output.get_mut(written..output_end).ok_or("PNG stored block exceeds output buffer")?;
            target.copy_from_slice(source);
            written = output_end;
            reader.bit = (offset + 4 + length) * 8;
            if final_block { break; }
            continue;
        }
        if kind == 1 {
            let mut lengths = vec![8u8; 288];
            lengths[144..256].fill(9);
            lengths[256..280].fill(7);
            previous = Some((table(&lengths), table(&[5u8; 32])));
        } else if kind == 2 {
            let literal_count = reader.take(5) + 257;
            let distance_count = reader.take(5) + 1;
            let code_count = reader.take(4) + 4;
            let mut code_lengths = [0u8; 19];
            for &index in &ORDER[..code_count] { code_lengths[index] = reader.take(3) as u8; }
            let code_table = table(&code_lengths);
            let total = literal_count + distance_count;
            let mut lengths = Vec::with_capacity(total);
            while lengths.len() < total {
                match reader.symbol(&code_table)? {
                    value @ 0..=15 => lengths.push(value as u8),
                    value @ 16..=18 => {
                        let (count, length) = match value {
                            16 => (reader.take(2) + 3, lengths.last().copied().unwrap_or(0)),
                            17 => (reader.take(3) + 3, 0),
                            _ => (reader.take(7) + 11, 0),
                        };
                        lengths.resize(lengths.len() + count, length);
                    }
                    _ => return Err("invalid PNG Huffman code length".into()),
                }
            }
            previous = Some((table(&lengths[..literal_count]), table(&lengths[literal_count..total])));
        }
        let (literal, distance) = previous.as_ref().ok_or("invalid PNG deflate block")?;
        loop {
            let symbol = reader.symbol(literal)?;
            if symbol < 256 {
                if let Some(target) = output.get_mut(written) { *target = symbol as u8; }
                written = written.checked_add(1).ok_or("PNG inflate overflow")?;
            } else if symbol == 256 {
                break;
            } else {
                let index = symbol - 257;
                let (length, extra) = if symbol <= 264 { (symbol - 254, 0) }
                    else { (*LENGTH_BASE.get(index).unwrap_or(&0), *LENGTH_EXTRA.get(index).unwrap_or(&0)) };
                let length = length + reader.take(extra);
                let code = reader.symbol(distance)?;
                let back = DIST_BASE.get(code).copied().unwrap_or(0) + reader.take(DIST_EXTRA.get(code).copied().unwrap_or(0));
                let end = written.checked_add(length).ok_or("PNG inflate overflow")?;
                while written < end {
                    for _ in 0..4 {
                        let value = written.checked_sub(back).and_then(|index| output.get(index)).copied().unwrap_or(0);
                        if let Some(target) = output.get_mut(written) { *target = value; }
                        written = written.checked_add(1).ok_or("PNG inflate overflow")?;
                    }
                }
                written = end;
            }
        }
        if final_block { break; }
    }
    output.truncate(written);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stored_block_ignores_nlen_and_zlib_header() {
        assert_eq!(decode(&[0, 0, 1, 3, 0, 0, 0, 10, 20, 30, 0, 0, 0, 0], 3).unwrap(), [10, 20, 30]);
    }
    #[test]
    fn empty_fixed_block_returns_original_zero_buffer() {
        assert_eq!(decode(&[0, 0, 3, 0, 0, 0, 0, 0], 4).unwrap(), [0; 4]);
    }
    #[test]
    fn compressed_huffman_blocks_decode_expected_bytes() {
        use std::io::Write;
        let payload = b"abcabcabcabc defdefdefdef abcabcabcabc";
        let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(payload).unwrap();
        let encoded = encoder.finish().unwrap();
        let mut wrapped = vec![0x78, 0x9c];
        wrapped.extend_from_slice(&encoded);
        wrapped.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(decode(&wrapped, payload.len()).unwrap(), payload);
    }
}
