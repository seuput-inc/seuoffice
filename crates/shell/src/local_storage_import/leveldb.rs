//! A reader of a closed LevelDB folder: the part of the format Chromium's
//! localStorage store uses (log files, table files, the manifest, snappy
//! blocks). Read-only; it never opens the folder the way LevelDB does, so a
//! copy of the folder is safe to read and the original is never touched.
//!
//! The live state of the store is the newest sequence of every key across the
//! tables the manifest lists and the logs it has not retired. A tombstone is
//! the newest sequence of its key, so a deleted key does not come back from
//! an older table. Checksums are not verified: a torn record at the end of a
//! log ends the log, any other malformed byte is an error.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

const BLOCK_SIZE: usize = 32 * 1024;
const LOG_HEADER: usize = 7;
const FOOTER_LEN: usize = 48;
const BLOCK_TRAILER_LEN: usize = 5;
const TABLE_MAGIC: u64 = 0xdb47_7524_8b80_fb57;
/// The most one snappy byte of input can expand to (a 3-byte copy of 64 bytes
/// is about 21); a declared length beyond it is corrupt, not a reason to allocate.
const SNAPPY_MAX_EXPANSION: usize = 32;

const KIND_DELETION: u8 = 0;
const KIND_VALUE: u8 = 1;

/// The live `key -> value` pairs of the LevelDB folder `dir`.
pub fn read_live(dir: &Path) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, String> {
    let current =
        fs::read_to_string(dir.join("CURRENT")).map_err(|error| format!("CURRENT: {error}"))?;
    let manifest_name = current.trim();
    if manifest_name.is_empty() || manifest_name.contains(['/', '\\']) {
        return Err(format!("CURRENT names '{manifest_name}'"));
    }
    let manifest =
        fs::read(dir.join(manifest_name)).map_err(|error| format!("{manifest_name}: {error}"))?;
    let version = Version::from_manifest(&manifest)?;

    let mut newest = Newest::default();
    for &number in &version.tables {
        let (name, bytes) = read_table_file(dir, number)?;
        read_table(&bytes, &mut newest).map_err(|error| format!("{name}: {error}"))?;
    }
    for (number, name) in logs_of(dir)? {
        if number < version.log_number && number != version.prev_log_number {
            continue;
        }
        let bytes = fs::read(dir.join(&name)).map_err(|error| format!("{name}: {error}"))?;
        read_log(&bytes, &mut newest).map_err(|error| format!("{name}: {error}"))?;
    }
    Ok(newest.live())
}

/// The newest sequence seen for every user key; `None` is a tombstone.
#[derive(Default)]
struct Newest {
    entries: HashMap<Vec<u8>, (u64, Option<Vec<u8>>)>,
}

impl Newest {
    fn offer(&mut self, key: &[u8], sequence: u64, value: Option<&[u8]>) {
        match self.entries.get_mut(key) {
            Some(slot) => {
                if slot.0 < sequence {
                    *slot = (sequence, value.map(<[u8]>::to_vec));
                }
            }
            None => {
                self.entries
                    .insert(key.to_vec(), (sequence, value.map(<[u8]>::to_vec)));
            }
        }
    }

    fn live(self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        self.entries
            .into_iter()
            .filter_map(|(key, (_, value))| value.map(|value| (key, value)))
            .collect()
    }
}

/// What the manifest's edits add up to.
#[derive(Default)]
struct Version {
    tables: BTreeSet<u64>,
    /// logs numbered below it are already in the tables
    log_number: u64,
    prev_log_number: u64,
}

impl Version {
    fn from_manifest(manifest: &[u8]) -> Result<Self, String> {
        let mut version = Version::default();
        for record in log_records(manifest)? {
            version
                .apply_edit(&record)
                .ok_or_else(|| "a malformed manifest edit".to_owned())?;
        }
        Ok(version)
    }

    fn apply_edit(&mut self, edit: &[u8]) -> Option<()> {
        let mut at = 0;
        while at < edit.len() {
            match varint(edit, &mut at)? {
                // comparator name
                1 => {
                    prefixed(edit, &mut at)?;
                }
                2 => self.log_number = varint(edit, &mut at)?,
                // next file number, last sequence
                3 | 4 => {
                    varint(edit, &mut at)?;
                }
                // compact pointer: level, key
                5 => {
                    varint(edit, &mut at)?;
                    prefixed(edit, &mut at)?;
                }
                // deleted file: level, number
                6 => {
                    varint(edit, &mut at)?;
                    let number = varint(edit, &mut at)?;
                    self.tables.remove(&number);
                }
                // new file: level, number, size, smallest key, largest key
                7 => {
                    varint(edit, &mut at)?;
                    let number = varint(edit, &mut at)?;
                    varint(edit, &mut at)?;
                    prefixed(edit, &mut at)?;
                    prefixed(edit, &mut at)?;
                    self.tables.insert(number);
                }
                9 => self.prev_log_number = varint(edit, &mut at)?,
                _ => return None,
            }
        }
        Some(())
    }
}

fn read_table_file(dir: &Path, number: u64) -> Result<(String, Vec<u8>), String> {
    for extension in ["ldb", "sst"] {
        let name = format!("{number:06}.{extension}");
        match fs::read(dir.join(&name)) {
            Ok(bytes) => return Ok((name, bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{name}: {error}")),
        }
    }
    Err(format!("the manifest lists table {number:06}, which is missing"))
}

/// `(number, file name)` of every log in `dir`, oldest first
fn logs_of(dir: &Path) -> Result<Vec<(u64, String)>, String> {
    let mut logs = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(number) = name
            .strip_suffix(".log")
            .and_then(|stem| stem.parse::<u64>().ok())
        {
            logs.push((number, name));
        }
    }
    logs.sort();
    Ok(logs)
}

/// The records of a log file (the manifest has the same framing). A torn
/// record at the end is dropped.
fn log_records(data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut records = Vec::new();
    let mut pending: Option<Vec<u8>> = None;
    let mut at = 0;
    while at < data.len() {
        let room = BLOCK_SIZE - at % BLOCK_SIZE;
        if room < LOG_HEADER {
            at += room;
            continue;
        }
        if at + LOG_HEADER > data.len() {
            break;
        }
        let length = usize::from(u16::from_le_bytes([data[at + 4], data[at + 5]]));
        let kind = data[at + 6];
        let start = at + LOG_HEADER;
        if kind == 0 && length == 0 {
            // zero padding up to the end of the block
            at += room;
            continue;
        }
        if start + length > data.len() {
            break;
        }
        if length > room - LOG_HEADER {
            return Err("a log record crosses its block".to_owned());
        }
        let fragment = &data[start..start + length];
        match (kind, pending.take()) {
            (1, None) => records.push(fragment.to_vec()),
            (2, None) => pending = Some(fragment.to_vec()),
            (3, Some(mut so_far)) => {
                so_far.extend_from_slice(fragment);
                pending = Some(so_far);
            }
            (4, Some(mut so_far)) => {
                so_far.extend_from_slice(fragment);
                records.push(so_far);
            }
            _ => return Err("a log record out of sequence".to_owned()),
        }
        at = start + length;
    }
    Ok(records)
}

/// A log file is a run of write batches.
fn read_log(data: &[u8], newest: &mut Newest) -> Result<(), String> {
    for record in log_records(data)? {
        read_batch(&record, newest)?;
    }
    Ok(())
}

/// One write batch: sequence, count, then `put key value` / `delete key` ops.
fn read_batch(record: &[u8], newest: &mut Newest) -> Result<(), String> {
    const MALFORMED: &str = "a malformed write batch";
    let sequence = le_u64(record.get(..8).ok_or(MALFORMED)?);
    let count = le_u64(record.get(8..12).ok_or(MALFORMED)?);
    let mut at = 12;
    for index in 0..count {
        let tag = *record.get(at).ok_or(MALFORMED)?;
        at += 1;
        let key = prefixed(record, &mut at).ok_or(MALFORMED)?;
        let sequence = sequence.wrapping_add(index);
        match tag {
            KIND_VALUE => {
                let value = prefixed(record, &mut at).ok_or(MALFORMED)?;
                newest.offer(key, sequence, Some(value));
            }
            KIND_DELETION => newest.offer(key, sequence, None),
            _ => return Err(MALFORMED.to_owned()),
        }
    }
    if at != record.len() {
        return Err(MALFORMED.to_owned());
    }
    Ok(())
}

/// One table file: the footer's index block names the data blocks, whose
/// entries are internal keys (user key, then sequence and kind in 8 bytes).
fn read_table(data: &[u8], newest: &mut Newest) -> Result<(), String> {
    const MALFORMED: &str = "a malformed table";
    let footer = data
        .len()
        .checked_sub(FOOTER_LEN)
        .map(|start| &data[start..])
        .ok_or(MALFORMED)?;
    if le_u64(&footer[FOOTER_LEN - 8..]) != TABLE_MAGIC {
        return Err("not a table file".to_owned());
    }
    let mut at = 0;
    let _metaindex = block_handle(footer, &mut at).ok_or(MALFORMED)?;
    let index = block_handle(footer, &mut at).ok_or(MALFORMED)?;
    let index_block = read_block(data, index)?;
    for_each_entry(&index_block, |_, handle| {
        let mut at = 0;
        let handle = block_handle(handle, &mut at).ok_or(MALFORMED)?;
        let block = read_block(data, handle)?;
        for_each_entry(&block, |internal_key, value| {
            let split = internal_key.len().checked_sub(8).ok_or(MALFORMED)?;
            let (user_key, trailer) = internal_key.split_at(split);
            let tag = le_u64(trailer);
            let sequence = tag >> 8;
            match u8::try_from(tag & 0xff) {
                Ok(KIND_VALUE) => newest.offer(user_key, sequence, Some(value)),
                Ok(KIND_DELETION) => newest.offer(user_key, sequence, None),
                _ => return Err(MALFORMED.to_owned()),
            }
            Ok(())
        })
    })
}

fn block_handle(data: &[u8], at: &mut usize) -> Option<(usize, usize)> {
    let offset = usize::try_from(varint(data, at)?).ok()?;
    let size = usize::try_from(varint(data, at)?).ok()?;
    Some((offset, size))
}

/// The contents of the block at `(offset, size)`, decompressed
fn read_block(data: &[u8], (offset, size): (usize, usize)) -> Result<Cow<'_, [u8]>, String> {
    const OUT_OF_RANGE: &str = "a block beyond the file";
    let end = offset.checked_add(size).ok_or(OUT_OF_RANGE)?;
    if end.checked_add(BLOCK_TRAILER_LEN).is_none_or(|n| n > data.len()) {
        return Err(OUT_OF_RANGE.to_owned());
    }
    let raw = &data[offset..end];
    match data[end] {
        0 => Ok(Cow::Borrowed(raw)),
        1 => snappy_decompress(raw)
            .map(Cow::Owned)
            .ok_or_else(|| "a corrupt snappy block".to_owned()),
        other => Err(format!("block compression {other}")),
    }
}

/// Calls `visit(key, value)` for every entry of a block: shared-prefix keys,
/// then the restart array that closes the block.
fn for_each_entry(
    block: &[u8],
    mut visit: impl FnMut(&[u8], &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    const MALFORMED: &str = "a malformed block";
    let restarts = usize::try_from(le_u64(
        block
            .len()
            .checked_sub(4)
            .map(|start| &block[start..])
            .ok_or(MALFORMED)?,
    ))
    .map_err(|_| MALFORMED)?;
    let tail = restarts
        .checked_mul(4)
        .and_then(|bytes| bytes.checked_add(4))
        .filter(|&tail| tail <= block.len())
        .ok_or(MALFORMED)?;
    let entries = &block[..block.len() - tail];
    let mut key: Vec<u8> = Vec::new();
    let mut at = 0;
    while at < entries.len() {
        let shared = usize::try_from(varint(entries, &mut at).ok_or(MALFORMED)?)
            .map_err(|_| MALFORMED)?;
        let added = usize::try_from(varint(entries, &mut at).ok_or(MALFORMED)?)
            .map_err(|_| MALFORMED)?;
        let value_len = usize::try_from(varint(entries, &mut at).ok_or(MALFORMED)?)
            .map_err(|_| MALFORMED)?;
        if shared > key.len() {
            return Err(MALFORMED.to_owned());
        }
        let key_end = at.checked_add(added).ok_or(MALFORMED)?;
        let value_end = key_end.checked_add(value_len).ok_or(MALFORMED)?;
        let delta = entries.get(at..key_end).ok_or(MALFORMED)?;
        let value = entries.get(key_end..value_end).ok_or(MALFORMED)?;
        key.truncate(shared);
        key.extend_from_slice(delta);
        visit(&key, value)?;
        at = value_end;
    }
    Ok(())
}

/// Snappy's raw format: the length, then literals and copies.
pub fn snappy_decompress(input: &[u8]) -> Option<Vec<u8>> {
    let mut at = 0;
    let expected = usize::try_from(varint(input, &mut at)?).ok()?;
    if expected > input.len().saturating_mul(SNAPPY_MAX_EXPANSION) {
        return None;
    }
    let mut out = Vec::with_capacity(expected);
    while at < input.len() {
        let tag = input[at];
        at += 1;
        let (length, offset) = match tag & 3 {
            0 => {
                let mut length = u64::from(tag >> 2);
                if length >= 60 {
                    let extra = usize::try_from(length - 59).ok()?;
                    length = le_u64(input.get(at..at + extra)?);
                    at += extra;
                }
                let length = usize::try_from(length).ok()?.checked_add(1)?;
                let literal = input.get(at..at.checked_add(length)?)?;
                if out.len() + length > expected {
                    return None;
                }
                out.extend_from_slice(literal);
                at += length;
                continue;
            }
            1 => {
                let low = usize::from(*input.get(at)?);
                at += 1;
                (
                    4 + usize::from((tag >> 2) & 7),
                    (usize::from(tag >> 5) << 8) | low,
                )
            }
            2 => {
                let offset = usize::try_from(le_u64(input.get(at..at + 2)?)).ok()?;
                at += 2;
                (1 + usize::from(tag >> 2), offset)
            }
            _ => {
                let offset = usize::try_from(le_u64(input.get(at..at + 4)?)).ok()?;
                at += 4;
                (1 + usize::from(tag >> 2), offset)
            }
        };
        if offset == 0 || offset > out.len() || out.len() + length > expected {
            return None;
        }
        // a copy may overlap its own output, so it goes byte by byte
        let from = out.len() - offset;
        for i in 0..length {
            let byte = out[from + i];
            out.push(byte);
        }
    }
    (out.len() == expected).then_some(out)
}

/// A little-endian integer of up to 8 bytes
fn le_u64(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .rev()
        .fold(0, |value, &byte| (value << 8) | u64::from(byte))
}

fn varint(data: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *data.get(*at)?;
        *at += 1;
        if shift >= 64 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
}

/// A varint length, then that many bytes
fn prefixed<'a>(data: &'a [u8], at: &mut usize) -> Option<&'a [u8]> {
    let length = usize::try_from(varint(data, at)?).ok()?;
    let end = at.checked_add(length)?;
    let bytes = data.get(*at..end)?;
    *at = end;
    Some(bytes)
}

#[cfg(test)]
pub mod fixture;

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;

    fn key(text: &str) -> Vec<u8> {
        text.as_bytes().to_vec()
    }

    fn put(text: &str, sequence: u64, value: &str) -> TableEntry {
        (key(text), sequence, Some(key(value)))
    }

    fn gone(text: &str, sequence: u64) -> TableEntry {
        (key(text), sequence, None)
    }

    fn batch(first_sequence: u64, ops: &[(&str, Option<&str>)]) -> Batch {
        Batch {
            first_sequence,
            ops: ops
                .iter()
                .map(|(k, v)| (key(k), v.map(key)))
                .collect(),
        }
    }

    fn pairs(db: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<(String, String)> {
        db.iter()
            .map(|(k, v)| {
                (
                    String::from_utf8_lossy(k).into_owned(),
                    String::from_utf8_lossy(v).into_owned(),
                )
            })
            .collect()
    }

    fn expected(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn only_the_newest_sequence_of_a_key_is_live() {
        for compressed in [false, true] {
            let dir = TempDir::new();
            write_db(
                dir.path(),
                &Fixture {
                    tables: vec![
                        TableSpec {
                            number: 5,
                            compressed,
                            entries: vec![
                                put("a", 1, "old"),
                                put("b", 2, "kept"),
                                put("c", 3, "doomed"),
                                put("d", 4, "from-table"),
                                put("e", 5, "e-old"),
                            ],
                        },
                        TableSpec {
                            number: 6,
                            compressed,
                            entries: vec![put("a", 20, "newer"), gone("d", 21)],
                        },
                    ],
                    batches: vec![
                        batch(30, &[("a", Some("newest")), ("c", None)]),
                        batch(40, &[("f", Some("logged")), ("e", None), ("e", Some("e-new"))]),
                    ],
                    ..Fixture::default()
                },
            );
            let live = read_live(dir.path()).unwrap();
            assert_eq!(
                pairs(&live),
                expected(&[
                    ("a", "newest"),
                    ("b", "kept"),
                    ("e", "e-new"),
                    ("f", "logged")
                ]),
                "compressed: {compressed}"
            );
        }
    }

    #[test]
    fn a_table_the_manifest_dropped_and_a_log_it_retired_are_ignored() {
        let dir = TempDir::new();
        write_db(
            dir.path(),
            &Fixture {
                tables: vec![TableSpec {
                    number: 8,
                    compressed: true,
                    entries: vec![put("live", 10, "yes")],
                }],
                stale_tables: vec![TableSpec {
                    number: 4,
                    compressed: false,
                    entries: vec![put("ghost", 2, "resurrected"), put("live", 3, "stale")],
                }],
                stale_log: vec![batch(1, &[("ghost-log", Some("resurrected"))])],
                ..Fixture::default()
            },
        );
        assert!(dir.path().join("000004.ldb").is_file());
        assert!(dir.path().join("000003.log").is_file());
        let live = read_live(dir.path()).unwrap();
        assert_eq!(pairs(&live), expected(&[("live", "yes")]));
    }

    #[test]
    fn a_batch_larger_than_a_block_is_reassembled_and_a_torn_tail_is_dropped() {
        let big: String = (0..70_000)
            .map(|i| char::from(b'a' + u8::try_from(i % 26).unwrap()))
            .collect();
        let dir = TempDir::new();
        write_db(
            dir.path(),
            &Fixture {
                batches: vec![
                    batch(1, &[("small", Some("one")), ("big", Some(&big))]),
                    batch(10, &[("torn", Some("never complete"))]),
                ],
                ..Fixture::default()
            },
        );
        let log = dir.path().join("000009.log");
        let bytes = fs::read(&log).unwrap();
        // the second record is a few bytes short, as after a crash mid-write
        fs::write(&log, &bytes[..bytes.len() - 4]).unwrap();
        let live = read_live(dir.path()).unwrap();
        assert_eq!(live.get(&key("small")), Some(&key("one")));
        assert_eq!(live.get(&key("big")).map(Vec::as_slice), Some(big.as_bytes()));
        assert!(!live.contains_key(&key("torn")));
    }

    #[test]
    fn a_missing_table_is_an_error_not_a_smaller_store() {
        let dir = TempDir::new();
        write_db(
            dir.path(),
            &Fixture {
                tables: vec![TableSpec {
                    number: 5,
                    compressed: false,
                    entries: vec![put("a", 1, "1")],
                }],
                ..Fixture::default()
            },
        );
        fs::remove_file(dir.path().join("000005.ldb")).unwrap();
        assert!(read_live(dir.path()).unwrap_err().contains("000005"));
    }

    #[test]
    fn a_folder_without_current_is_an_error() {
        let dir = TempDir::new();
        assert!(read_live(dir.path()).is_err());
    }

    #[test]
    fn snappy_literals_and_the_three_copy_forms_decode() {
        assert_eq!(
            snappy_decompress(&[0x0a, 0x00, b'a', 0x15, 0x01]).unwrap(),
            b"aaaaaaaaaa"
        );
        assert_eq!(
            snappy_decompress(&[0x0c, 0x0c, b'a', b'b', b'c', b'd', 0x1e, 0x04, 0x00]).unwrap(),
            b"abcdabcdabcd"
        );
        assert_eq!(
            snappy_decompress(&[0x0c, 0x0c, b'a', b'b', b'c', b'd', 0x1f, 0x04, 0, 0, 0]).unwrap(),
            b"abcdabcdabcd"
        );
        let mut long = vec![61, 0xf0, 60];
        long.extend(std::iter::repeat_n(b'x', 61));
        assert_eq!(snappy_decompress(&long).unwrap(), vec![b'x'; 61]);
    }

    #[test]
    fn corrupt_snappy_input_is_refused() {
        // the copy reaches before the start of the output
        assert!(snappy_decompress(&[0x05, 0x00, b'a', 0x01, 0x02]).is_none());
        // a zero offset
        assert!(snappy_decompress(&[0x05, 0x00, b'a', 0x01, 0x00]).is_none());
        // the output is shorter than declared
        assert!(snappy_decompress(&[0x05, 0x00, b'a']).is_none());
        // the output is longer than declared
        assert!(snappy_decompress(&[0x01, 0x10, b'a', b'b', b'c', b'd', b'e']).is_none());
        // a declared length no input could expand to
        assert!(snappy_decompress(&[0xff, 0xff, 0xff, 0x7f, 0x00, b'a']).is_none());
    }
}
