// The decoding algorithm follows pdf-lib's FlateStream, derived from PDF.js
// and Xpdf under the Apache License 2.0.

const LENGTH: [u32; 31] = [
    0x00003,0x00004,0x00005,0x00006,0x00007,0x00008,0x00009,0x0000a,
    0x1000b,0x1000d,0x1000f,0x10011,0x20013,0x20017,0x2001b,0x2001f,
    0x30023,0x3002b,0x30033,0x3003b,0x40043,0x40053,0x40063,0x40073,
    0x50083,0x500a3,0x500c3,0x500e3,0x00102,0x00102,0x00102,
];
const DISTANCE: [u32; 30] = [
    0x00001,0x00002,0x00003,0x00004,0x10005,0x10007,0x20009,0x2000d,
    0x30011,0x30019,0x40021,0x40031,0x50041,0x50061,0x60081,0x600c1,
    0x70101,0x70181,0x80201,0x80301,0x90401,0x90601,0xa0801,0xa0c01,
    0xb1001,0xb1801,0xc2001,0xc3001,0xd4001,0xd6001,
];
const ORDER: [usize; 19] = [16,17,18,0,8,7,9,6,10,5,11,4,12,3,13,2,14,1,15];

type Table = (Vec<u32>, u32);

fn table(lengths: &[u8]) -> Table {
    let max = u32::from(lengths.iter().copied().max().unwrap_or(0));
    let size = 1usize << max;
    let mut codes = vec![0u32; size];
    let mut code = 0u32;
    for len in 1..=max {
        let skip = 1usize << len;
        for (value, &length) in lengths.iter().enumerate() {
            if u32::from(length) != len { continue; }
            let mut reverse = 0u32;
            let mut bits = code;
            for _ in 0..len { reverse = (reverse << 1) | (bits & 1); bits >>= 1; }
            let mut index = reverse as usize;
            while index < size {
                codes[index] = (len << 16) | value as u32;
                index += skip;
            }
            code += 1;
        }
        code <<= 1;
    }
    (codes, max)
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
    code_buffer: u32,
    code_size: u32,
    output: Vec<u8>,
    eof: bool,
}

impl Decoder<'_> {
    fn byte(&mut self) -> Option<u8> {
        let value = self.bytes.get(self.position).copied();
        if value.is_some() { self.position += 1; }
        value
    }

    fn bits(&mut self, count: u32) -> Result<u32, String> {
        while self.code_size < count {
            let byte = self.byte().ok_or("Bad encoding in flate stream")?;
            self.code_buffer |= u32::from(byte) << self.code_size;
            self.code_size += 8;
        }
        let result = self.code_buffer & ((1 << count) - 1);
        self.code_buffer >>= count;
        self.code_size -= count;
        Ok(result)
    }

    fn code(&mut self, table: &Table) -> Result<u32, String> {
        let (codes, max) = table;
        while self.code_size < *max {
            let Some(byte) = self.byte() else { break };
            self.code_buffer |= u32::from(byte) << self.code_size;
            self.code_size += 8;
        }
        let packed = codes[(self.code_buffer & ((1u32 << *max) - 1)) as usize];
        let length = packed >> 16;
        if length == 0 || self.code_size < length { return Err("Bad encoding in flate stream".into()); }
        self.code_buffer >>= length;
        self.code_size -= length;
        Ok(packed & 0xffff)
    }

    fn block(&mut self) -> Result<(), String> {
        let header = self.bits(3)?;
        if header & 1 != 0 { self.eof = true; }
        match header >> 1 {
            0 => {
                let lo = self.byte().ok_or("Bad block header in flate stream")?;
                let hi = self.byte().ok_or("Bad block header in flate stream")?;
                let cl = self.byte().ok_or("Bad block header in flate stream")?;
                let ch = self.byte().ok_or("Bad block header in flate stream")?;
                let length = usize::from(lo) | (usize::from(hi) << 8);
                let check = usize::from(cl) | (usize::from(ch) << 8);
                if check != (!length & 0xffff) && (length != 0 || check != 0) {
                    return Err("Bad uncompressed block length in flate stream".into());
                }
                self.code_buffer = 0;
                self.code_size = 0;
                if length == 0 && self.position == self.bytes.len() { self.eof = true; }
                let start = self.output.len();
                self.output.resize(start + length, 0);
                for index in 0..length {
                    match self.byte() {
                        Some(byte) => self.output[start + index] = byte,
                        None => { self.eof = true; break; }
                    }
                }
                Ok(())
            }
            kind @ (1 | 2) => {
                let (literal, distance) = if kind == 1 {
                    let mut lengths = vec![8u8; 288];
                    lengths[144..256].fill(9);
                    lengths[256..280].fill(7);
                    (table(&lengths), table(&[5u8; 30]))
                } else {
                    let literal_count = self.bits(5)? as usize + 257;
                    let distance_count = self.bits(5)? as usize + 1;
                    let code_count = self.bits(4)? as usize + 4;
                    let mut lengths = [0u8; 19];
                    for &index in &ORDER[..code_count] { lengths[index] = self.bits(3)? as u8; }
                    let code_table = table(&lengths);
                    let total = literal_count + distance_count;
                    let mut lengths = vec![0u8; total];
                    let mut index = 0usize;
                    let mut last = 0u8;
                    while index < total {
                        let code = self.code(&code_table)?;
                        let (bits, base, value) = match code {
                            16 => (2, 3, last),
                            17 => { last = 0; (3, 3, 0) },
                            18 => { last = 0; (7, 11, 0) },
                            other => { last = other as u8; lengths[index] = last; index += 1; continue; }
                        };
                        let repeats = self.bits(bits)? as usize + base;
                        for _ in 0..repeats {
                            if let Some(slot) = lengths.get_mut(index) { *slot = value; }
                            index += 1;
                        }
                    }
                    (table(&lengths[..literal_count]), table(&lengths[literal_count..]))
                };
                loop {
                    let code = self.code(&literal)?;
                    if code < 256 { self.output.push(code as u8); continue; }
                    if code == 256 { return Ok(()); }
                    let packed = LENGTH.get(code as usize - 257).copied().unwrap_or(0);
                    let extra = packed >> 16;
                    let length = (packed & 0xffff) + if extra > 0 { self.bits(extra)? } else { 0 };
                    let code = self.code(&distance)?;
                    let packed = DISTANCE.get(code as usize).copied().unwrap_or(0);
                    let extra = packed >> 16;
                    let distance = (packed & 0xffff) + if extra > 0 { self.bits(extra)? } else { 0 };
                    for _ in 0..length {
                        let value = self.output.len().checked_sub(distance as usize)
                            .and_then(|at| self.output.get(at)).copied().unwrap_or(0);
                        self.output.push(value);
                    }
                }
            }
            _ => Err("Unknown block type in flate stream".into()),
        }
    }
}

pub(super) fn decode(input: &[u8]) -> Result<Vec<u8>, String> {
    if input.len() < 2 || input[0] & 15 != 8 || ((u16::from(input[0]) << 8) + u16::from(input[1])) % 31 != 0 || input[1] & 0x20 != 0 {
        return Err("invalid PDF flate header".into());
    }
    let mut decoder = Decoder { bytes: input, position: 2, code_buffer: 0, code_size: 0, output: Vec::new(), eof: false };
    while !decoder.eof { decoder.block()?; }
    Ok(decoder.output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncompressed_truncation_pads_and_bad_header_fails() {
        assert_eq!(decode(&[0x78,0x01,0x01,0x03,0x00,0xfc,0xff,b'A']).unwrap(), [b'A',0,0]);
        assert!(decode(&[0,0]).is_err());
    }
}
