//! A writer of small LevelDB folders for the tests: tables with prefix-shared
//! keys and several restarts, optionally snappy-framed, logs that span blocks,
//! and a manifest with a dropped table and a retired log.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{BLOCK_SIZE, LOG_HEADER, TABLE_MAGIC};

/// `(user key, sequence, value)`; `None` is a tombstone
pub type TableEntry = (Vec<u8>, u64, Option<Vec<u8>>);

pub struct TableSpec {
    pub number: u64,
    pub compressed: bool,
    pub entries: Vec<TableEntry>,
}

pub struct Batch {
    pub first_sequence: u64,
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

#[derive(Default)]
pub struct Fixture {
    /// listed by the manifest
    pub tables: Vec<TableSpec>,
    /// on disk, but the manifest's last edit deleted them
    pub stale_tables: Vec<TableSpec>,
    /// the live log, `000009.log`
    pub batches: Vec<Batch>,
    /// a log the manifest retired, `000003.log`
    pub stale_log: Vec<Batch>,
}

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "seu-ls-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Default for TempDir {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn write_db(dir: &Path, fixture: &Fixture) {
    fs::create_dir_all(dir).unwrap();
    let mut new_files = Vec::new();
    for spec in fixture.tables.iter().chain(&fixture.stale_tables) {
        let bytes = build_table(spec);
        fs::write(dir.join(format!("{:06}.ldb", spec.number)), &bytes).unwrap();
        new_files.push((spec.number, bytes.len() as u64));
    }
    fs::write(dir.join("000009.log"), build_log(&encode_batches(&fixture.batches))).unwrap();
    if !fixture.stale_log.is_empty() {
        fs::write(
            dir.join("000003.log"),
            build_log(&encode_batches(&fixture.stale_log)),
        )
        .unwrap();
    }

    let mut first = Vec::new();
    put_varint(&mut first, 1);
    put_prefixed(&mut first, b"leveldb.BytewiseComparator");
    put_varint(&mut first, 2);
    put_varint(&mut first, 3);
    put_varint(&mut first, 3);
    put_varint(&mut first, 20);
    put_varint(&mut first, 4);
    put_varint(&mut first, 100);
    for (number, size) in new_files {
        put_varint(&mut first, 7);
        put_varint(&mut first, 0);
        put_varint(&mut first, number);
        put_varint(&mut first, size);
        let mut bound = b"a".to_vec();
        bound.extend_from_slice(&[0; 8]);
        put_prefixed(&mut first, &bound);
        put_prefixed(&mut first, &bound);
    }
    let mut second = Vec::new();
    put_varint(&mut second, 2);
    put_varint(&mut second, 9);
    put_varint(&mut second, 9);
    put_varint(&mut second, 0);
    for spec in &fixture.stale_tables {
        put_varint(&mut second, 6);
        put_varint(&mut second, 0);
        put_varint(&mut second, spec.number);
    }
    fs::write(dir.join("MANIFEST-000002"), build_log(&[first, second])).unwrap();
    fs::write(dir.join("CURRENT"), "MANIFEST-000002\n").unwrap();
}

fn encode_batches(batches: &[Batch]) -> Vec<Vec<u8>> {
    batches
        .iter()
        .map(|batch| {
            let mut record = batch.first_sequence.to_le_bytes().to_vec();
            record.extend_from_slice(&(batch.ops.len() as u32).to_le_bytes());
            for (key, value) in &batch.ops {
                match value {
                    Some(value) => {
                        record.push(1);
                        put_prefixed(&mut record, key);
                        put_prefixed(&mut record, value);
                    }
                    None => {
                        record.push(0);
                        put_prefixed(&mut record, key);
                    }
                }
            }
            record
        })
        .collect()
}

/// Records framed into 32 KiB blocks: full, first, middle and last fragments.
fn build_log(records: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for record in records {
        let mut left = record.as_slice();
        let mut first = true;
        loop {
            let room = BLOCK_SIZE - out.len() % BLOCK_SIZE;
            if room < LOG_HEADER {
                out.extend(std::iter::repeat_n(0, room));
                continue;
            }
            let length = left.len().min(room - LOG_HEADER);
            let last = length == left.len();
            let kind = match (first, last) {
                (true, true) => 1,
                (true, false) => 2,
                (false, true) => 4,
                (false, false) => 3,
            };
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&(length as u16).to_le_bytes());
            out.push(kind);
            out.extend_from_slice(&left[..length]);
            left = &left[length..];
            first = false;
            if last {
                break;
            }
        }
    }
    out
}

fn build_table(spec: &TableSpec) -> Vec<u8> {
    let mut entries: Vec<&TableEntry> = spec.entries.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut file = Vec::new();
    let mut index = Vec::new();
    for chunk in entries.chunks(3) {
        let block: Vec<(Vec<u8>, Vec<u8>)> = chunk
            .iter()
            .map(|(key, sequence, value)| {
                let kind = u64::from(value.is_some());
                let mut internal = key.clone();
                internal.extend_from_slice(&((sequence << 8) | kind).to_le_bytes());
                (internal, value.clone().unwrap_or_default())
            })
            .collect();
        let (offset, size) = append_block(&mut file, &build_block(&block, 2), spec.compressed);
        let mut handle = Vec::new();
        put_varint(&mut handle, offset);
        put_varint(&mut handle, size);
        let last_key = block.last().map(|(key, _)| key.clone()).unwrap_or_default();
        index.push((last_key, handle));
    }
    let metaindex = append_block(&mut file, &build_block(&[], 1), false);
    let index = append_block(&mut file, &build_block(&index, 1), false);
    let mut footer = Vec::new();
    put_varint(&mut footer, metaindex.0);
    put_varint(&mut footer, metaindex.1);
    put_varint(&mut footer, index.0);
    put_varint(&mut footer, index.1);
    footer.resize(40, 0);
    footer.extend_from_slice(&TABLE_MAGIC.to_le_bytes());
    file.extend_from_slice(&footer);
    file
}

/// Appends a block and its trailer (kind, checksum); `(offset, size)` of the block
fn append_block(file: &mut Vec<u8>, contents: &[u8], compressed: bool) -> (u64, u64) {
    let offset = file.len() as u64;
    let (payload, kind) = if compressed {
        (snappy_literals(contents), 1)
    } else {
        (contents.to_vec(), 0)
    };
    file.extend_from_slice(&payload);
    file.push(kind);
    file.extend_from_slice(&[0; 4]);
    (offset, payload.len() as u64)
}

/// A block: entries with shared key prefixes, a restart every `interval`
/// entries, then the restart array and its length.
fn build_block(entries: &[(Vec<u8>, Vec<u8>)], interval: usize) -> Vec<u8> {
    let mut block = Vec::new();
    let mut restarts = vec![0u32];
    let mut previous: &[u8] = &[];
    let mut since_restart = 0;
    for (key, value) in entries {
        let shared = if since_restart < interval {
            previous.iter().zip(key).take_while(|(a, b)| a == b).count()
        } else {
            restarts.push(block.len() as u32);
            since_restart = 0;
            0
        };
        put_varint(&mut block, shared as u64);
        put_varint(&mut block, (key.len() - shared) as u64);
        put_varint(&mut block, value.len() as u64);
        block.extend_from_slice(&key[shared..]);
        block.extend_from_slice(value);
        previous = key.as_slice();
        since_restart += 1;
    }
    for restart in &restarts {
        block.extend_from_slice(&restart.to_le_bytes());
    }
    block.extend_from_slice(&(restarts.len() as u32).to_le_bytes());
    block
}

/// Valid snappy made of literals only
fn snappy_literals(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, data.len() as u64);
    for chunk in data.chunks(50_000) {
        if chunk.len() <= 60 {
            out.push(((chunk.len() - 1) << 2) as u8);
        } else {
            out.push(61 << 2);
            out.extend_from_slice(&((chunk.len() - 1) as u16).to_le_bytes());
        }
        out.extend_from_slice(chunk);
    }
    out
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}
