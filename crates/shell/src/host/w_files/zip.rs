use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const EOCD: u32 = 0x0605_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const LOCAL: u32 = 0x0403_4b50;
const END_LEN: u64 = 22;
const TAIL_LEN: u64 = END_LEN + 0xffff;
const CENTRAL_LEN: usize = 46;
const LOCAL_LEN: u64 = 30;

#[derive(Clone, Copy)]
struct Entry<'a> {
    flags: u16,
    method: u16,
    time: u16,
    date: u16,
    crc: u32,
    compressed: u32,
    size: u32,
    local_offset: u32,
    name: &'a [u8],
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn read_at(reader: &mut (impl Read + Seek), position: u64, buf: &mut [u8]) -> Result<(), String> {
    reader
        .seek(SeekFrom::Start(position))
        .map_err(|e| e.to_string())?;
    reader.read_exact(buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            "zip: unexpected end of file".to_owned()
        } else {
            e.to_string()
        }
    })
}

fn bytes(length: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    out.try_reserve_exact(length)
        .map_err(|_| "zip: the copy does not fit in memory".to_owned())?;
    out.resize(length, 0);
    Ok(out)
}

fn entries(cd: &[u8], count: u16) -> Result<Vec<Entry<'_>>, String> {
    let mut result = Vec::with_capacity(count as usize);
    let mut at = 0usize;
    for _ in 0..count {
        let head = cd
            .get(at..at + CENTRAL_LEN)
            .ok_or("zip: broken central directory")?;
        if u32_at(head, 0) != CENTRAL {
            return Err("zip: broken central directory".into());
        }
        let name_len = usize::from(u16_at(head, 28));
        let end = at
            + CENTRAL_LEN
            + name_len
            + usize::from(u16_at(head, 30))
            + usize::from(u16_at(head, 32));
        if end > cd.len() {
            return Err("zip: broken central directory".into());
        }
        let compressed = u32_at(head, 20);
        let size = u32_at(head, 24);
        let local_offset = u32_at(head, 42);
        if compressed == u32::MAX || size == u32::MAX || local_offset == u32::MAX {
            return Err("zip: zip64 archives are not supported".into());
        }
        result.push(Entry {
            flags: u16_at(head, 8),
            method: u16_at(head, 10),
            time: u16_at(head, 12),
            date: u16_at(head, 14),
            crc: u32_at(head, 16),
            compressed,
            size,
            local_offset,
            name: &cd[at + CENTRAL_LEN..at + CENTRAL_LEN + name_len],
        });
        at = end;
    }
    Ok(result)
}

fn put16(buf: &mut [u8], at: usize, value: u16) {
    buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn field32(value: usize) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| {
        let digits = value.to_string();
        let mut grouped = String::new();
        for (i, c) in digits.chars().enumerate() {
            if value > 1usize << 32 && i > 0 && (digits.len() - i) % 3 == 0 { grouped.push('_'); }
            grouped.push(c);
        }
        format!("The value of \"value\" is out of range. It must be >= 0 and <= 4294967295. Received {grouped}")
    })
}

fn copy_text_parts(reader: &mut (impl Read + Seek), file_size: u64) -> Result<Vec<u8>, String> {
    if file_size < END_LEN {
        return Err("zip: file too short".into());
    }
    let tail_len = file_size.min(TAIL_LEN) as usize;
    let mut tail = bytes(tail_len)?;
    read_at(reader, file_size - tail_len as u64, &mut tail)?;
    let end_at = (0..=tail_len - END_LEN as usize)
        .rev()
        .find(|&i| u32_at(&tail, i) == EOCD)
        .ok_or("zip: end of central directory not found")?;
    let end = &tail[end_at..end_at + END_LEN as usize];
    let count = u16_at(end, 10);
    let cd_len = u32_at(end, 12);
    let cd_offset = u32_at(end, 16);
    if count == u16::MAX || cd_len == u32::MAX || cd_offset == u32::MAX {
        return Err("zip: zip64 archives are not supported".into());
    }
    if u64::from(cd_offset) + u64::from(cd_len) > file_size {
        return Err("zip: central directory out of range".into());
    }
    let mut directory = bytes(cd_len as usize)?;
    read_at(reader, u64::from(cd_offset), &mut directory)?;
    let entries = entries(&directory, count)?;
    let mut out = Vec::new();
    let mut central = Vec::new();
    for mut entry in entries {
        let keep = file_parse::zip_text_parts::is_text_part(&String::from_utf8_lossy(entry.name));
        let flags = if keep {
            entry.flags & !8
        } else {
            entry.flags & 0x0800
        };
        let mut data_start = 0;
        if keep {
            let offset = u64::from(entry.local_offset);
            if offset + LOCAL_LEN > file_size {
                return Err("zip: entry out of range".into());
            }
            let mut head = [0; 30];
            read_at(reader, offset, &mut head)?;
            if u32_at(&head, 0) != LOCAL {
                return Err("zip: broken local header".into());
            }
            data_start =
                offset + LOCAL_LEN + u64::from(u16_at(&head, 26)) + u64::from(u16_at(&head, 28));
            if data_start + u64::from(entry.compressed) > file_size {
                return Err("zip: entry out of range".into());
            }
        } else {
            entry.method = 0;
            entry.crc = 0;
            entry.compressed = 0;
            entry.size = 0;
        }
        let offset = field32(out.len())?;
        let part_len = 30usize + entry.name.len() + entry.compressed as usize;
        out.try_reserve(part_len)
            .map_err(|_| "zip: the copy does not fit in memory".to_owned())?;
        let mut local = [0; 30];
        put32(&mut local, 0, LOCAL);
        put16(&mut local, 4, 20);
        put16(&mut local, 6, flags);
        put16(&mut local, 8, entry.method);
        put16(&mut local, 10, entry.time);
        put16(&mut local, 12, entry.date);
        put32(&mut local, 14, entry.crc);
        put32(&mut local, 18, entry.compressed);
        put32(&mut local, 22, entry.size);
        put16(&mut local, 26, entry.name.len() as u16);
        out.extend_from_slice(&local);
        out.extend_from_slice(entry.name);
        if keep {
            let start = out.len();
            out.resize(start + entry.compressed as usize, 0);
            read_at(reader, data_start, &mut out[start..])?;
        }
        let mut head = [0; 46];
        put32(&mut head, 0, CENTRAL);
        put16(&mut head, 4, 20);
        put16(&mut head, 6, 20);
        put16(&mut head, 8, flags);
        put16(&mut head, 10, entry.method);
        put16(&mut head, 12, entry.time);
        put16(&mut head, 14, entry.date);
        put32(&mut head, 16, entry.crc);
        put32(&mut head, 20, entry.compressed);
        put32(&mut head, 24, entry.size);
        put16(&mut head, 28, entry.name.len() as u16);
        put32(&mut head, 42, offset);
        central.extend_from_slice(&head);
        central.extend_from_slice(entry.name);
    }
    let cd_offset = field32(out.len())?;
    let cd_len = field32(central.len())?;
    out.try_reserve(central.len() + END_LEN as usize)
        .map_err(|_| "zip: the copy does not fit in memory".to_owned())?;
    out.extend_from_slice(&central);
    let mut end = [0; 22];
    put32(&mut end, 0, EOCD);
    put16(&mut end, 8, count);
    put16(&mut end, 10, count);
    put32(&mut end, 12, cd_len);
    put32(&mut end, 16, cd_offset);
    out.extend_from_slice(&end);
    Ok(out)
}

pub(super) fn read_zip_text_parts(path: &Path) -> Result<Vec<u8>, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    copy_text_parts(&mut file, size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Tracked {
        inner: Cursor<Vec<u8>>,
        reads: Vec<(u64, u64)>,
    }

    impl Read for Tracked {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let start = self.inner.position();
            let count = self.inner.read(buf)?;
            self.reads.push((start, start + count as u64));
            Ok(count)
        }
    }

    impl Seek for Tracked {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    fn archive() -> (Vec<u8>, u64) {
        let media = vec![7; 160_000];
        let inputs: [(&[u8], &[u8]); 3] = [
            (b"media/image.png", &media),
            ("책.xml".as_bytes(), b"hello"),
            ("책.xml".as_bytes(), b"hello"),
        ];
        let mut data = Vec::new();
        let mut central = Vec::new();
        let mut media_start = 0;
        for (name, content) in inputs {
            let offset = data.len() as u32;
            let mut local = [0; 30];
            put32(&mut local, 0, LOCAL);
            put16(&mut local, 4, 20);
            put16(&mut local, 6, 0x0808);
            put16(&mut local, 10, 0x1234);
            put16(&mut local, 12, 0x5678);
            put32(&mut local, 14, 0x3610_a686);
            put32(&mut local, 18, content.len() as u32);
            put32(&mut local, 22, content.len() as u32);
            put16(&mut local, 26, name.len() as u16);
            data.extend_from_slice(&local);
            data.extend_from_slice(name);
            if offset == 0 {
                media_start = data.len() as u64;
            }
            data.extend_from_slice(content);
            let mut header = [0; 46];
            put32(&mut header, 0, CENTRAL);
            put16(&mut header, 4, 20);
            put16(&mut header, 6, 20);
            put16(&mut header, 8, 0x0808);
            put16(&mut header, 12, 0x1234);
            put16(&mut header, 14, 0x5678);
            put32(&mut header, 16, 0x3610_a686);
            put32(&mut header, 20, content.len() as u32);
            put32(&mut header, 24, content.len() as u32);
            put16(&mut header, 28, name.len() as u16);
            put32(&mut header, 42, offset);
            central.extend_from_slice(&header);
            central.extend_from_slice(name);
        }
        let cd_offset = data.len() as u32;
        data.extend_from_slice(&central);
        let mut end = [0; 22];
        put32(&mut end, 0, EOCD);
        put16(&mut end, 8, 3);
        put16(&mut end, 10, 3);
        put32(&mut end, 12, central.len() as u32);
        put32(&mut end, 16, cd_offset);
        data.extend_from_slice(&end);
        (data, media_start)
    }

    #[test]
    fn copies_text_without_reading_media_body_and_preserves_entries() {
        let (input, media_start) = archive();
        let size = input.len() as u64;
        let mut reader = Tracked {
            inner: Cursor::new(input),
            reads: Vec::new(),
        };
        let output = copy_text_parts(&mut reader, size).unwrap();
        assert!(
            reader
                .reads
                .iter()
                .all(|&(start, end)| end <= media_start || start >= media_start + 80_000)
        );
        let end = &output[output.len() - 22..];
        let offset = u32_at(end, 16) as usize;
        let copied = entries(&output[offset..output.len() - 22], u16_at(end, 10)).unwrap();
        assert_eq!(
            copied.iter().map(|e| e.name).collect::<Vec<_>>(),
            vec![
                b"media/image.png".as_slice(),
                "책.xml".as_bytes(),
                "책.xml".as_bytes()
            ]
        );
        assert_eq!(
            (copied[0].size, copied[0].compressed, copied[0].flags),
            (0, 0, 0x0800)
        );
        for entry in &copied[1..] {
            assert_eq!(
                (entry.time, entry.date, entry.flags, entry.size),
                (0x1234, 0x5678, 0x0800, 5)
            );
            let start = entry.local_offset as usize + 30 + entry.name.len();
            assert_eq!(&output[start..start + 5], b"hello");
        }
    }

    #[test]
    fn rejects_truncated_and_zip64_directories() {
        let mut short = Cursor::new(vec![0; 21]);
        assert_eq!(
            copy_text_parts(&mut short, 21).unwrap_err(),
            "zip: file too short"
        );
        let (mut input, _) = archive();
        let at = input.len() - 22;
        put16(&mut input, at + 10, u16::MAX);
        let size = input.len() as u64;
        assert_eq!(
            copy_text_parts(&mut Cursor::new(input), size).unwrap_err(),
            "zip: zip64 archives are not supported"
        );
    }
}
