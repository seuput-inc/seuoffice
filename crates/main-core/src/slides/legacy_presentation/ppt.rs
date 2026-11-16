use super::{Converted, PPT_MAX_BYTES, PPT_MAX_STREAM_BYTES, Stop};
use crate::packages::compound_file::{Failure, Ole2FailureCode, assert_compound_file};
use file_parse::compound::{IndexReadError, read_index_with_stream_limit};
use std::collections::{HashMap, HashSet};
use std::fmt;

const MAX_SLIDES: usize = 4096;
const MAX_DEPTH: usize = 64;
const MAX_CHAIN: usize = 1024;
const MAX_SEQUENCES: usize = 100_000;
const MAX_CHARS: usize = MAX_SLIDES * 1024;
const MAX_PARAGRAPHS: usize = MAX_SLIDES * 64;
const DOCUMENT: u16 = 0x03e8;
const DOCUMENT_ATOM: u16 = 0x03e9;
const SLIDE: u16 = 0x03ee;
const SLIDE_PERSIST: u16 = 0x03f3;
const TEXT_HEADER: u16 = 0x0f9f;
const TEXT_CHARS: u16 = 0x0fa0;
const TEXT_BYTES: u16 = 0x0fa8;
const SLIDE_LIST: u16 = 0x0ff0;
const USER_EDIT: u16 = 0x0ff5;
const CURRENT_USER: u16 = 0x0ff6;
const PERSIST: u16 = 0x1772;
const TEXTBOX: u16 = 0xf00d;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Error {
    pub code: &'static str,
    pub detail: Option<String>,
}
impl Error {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: Some(detail.into()),
        }
    }
    fn bare(code: &'static str) -> Self {
        Self { code, detail: None }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let advice = match self.code {
            "not_cfb" => "The file does not look like a PowerPoint 97-2003 presentation.",
            "truncated" => "The file is shorter than its own structure claims.",
            "missing_stream" => "The presentation stream is missing from the file.",
            "cancelled" => "conversion cancelled",
            "unsupported_version" => "Only PowerPoint 97-2003 (.ppt) files are supported.",
            "encrypted" => {
                "The presentation is password-protected; remove the password in PowerPoint before converting it."
            }
            "limits_exceeded" => "The presentation is too large to convert.",
            "bad_persist" => "The presentation change directory is damaged.",
            "bad_slide" => "A slide container is damaged.",
            _ => "The presentation structure is damaged beyond parsing.",
        };
        f.write_str(advice)?;
        if let Some(detail) = &self.detail {
            write!(f, " ({detail})")?;
        }
        Ok(())
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Sequence {
    pub kind: i32,
    pub text: String,
}
#[derive(Debug)]
pub(super) struct Presentation {
    pub slides: Vec<Vec<Sequence>>,
    pub width: u32,
    pub height: u32,
    pub warnings: Vec<String>,
}
#[derive(Clone, Copy)]
struct Record {
    version: u16,
    instance: u16,
    kind: u16,
    offset: usize,
    length: usize,
}
impl Record {
    fn end(self) -> usize {
        self.offset + self.length
    }
    fn payload(self, bytes: &[u8]) -> &[u8] {
        &bytes[self.offset..self.end()]
    }
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}
fn record_at(bytes: &[u8], at: usize) -> Option<Record> {
    let header = bytes.get(at..at.checked_add(8)?)?;
    let head = u16::from_le_bytes([header[0], header[1]]);
    let length = u32_at(header, 4) as usize;
    let offset = at + 8;
    bytes.get(offset..offset.checked_add(length)?)?;
    Some(Record {
        version: head & 15,
        instance: head >> 4,
        kind: u16::from_le_bytes([header[2], header[3]]),
        offset,
        length,
    })
}
struct Records<'a> {
    bytes: &'a [u8],
    at: usize,
    end: usize,
    cancelled: &'a dyn Fn() -> bool,
}
impl<'a> Records<'a> {
    fn new(bytes: &'a [u8], from: usize, end: usize, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self {
            bytes,
            at: from,
            end,
            cancelled,
        }
    }
    fn all(bytes: &'a [u8], cancelled: &'a dyn Fn() -> bool) -> Self {
        Self::new(bytes, 0, bytes.len(), cancelled)
    }
}
impl Iterator for Records<'_> {
    type Item = Result<Record, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.end.saturating_sub(self.at) < 8 {
            return None;
        }
        let at = self.at;
        if (self.cancelled)() {
            self.at = self.end;
            return Some(Err(Error::bare("cancelled")));
        }
        let header = &self.bytes[at..at + 8];
        let length = u32_at(header, 4) as usize;
        let kind = u16::from_le_bytes([header[2], header[3]]);
        if length > self.end - at - 8 {
            self.at = self.end;
            return Some(Err(Error::new(
                "truncated",
                format!("record {kind:x} claims {length} bytes"),
            )));
        }
        self.at += 8 + length;
        Some(Ok(Record {
            version: u16::from_le_bytes([header[0], header[1]]) & 15,
            instance: u16::from_le_bytes([header[0], header[1]]) >> 4,
            kind,
            offset: at + 8,
            length,
        }))
    }
}
fn current_info(bytes: &[u8], stop: &Stop<'_>) -> Result<Option<(u32, Option<usize>)>, Error> {
    for record in Records::all(bytes, stop.0) {
        let record = record?;
        if record.kind != CURRENT_USER {
            continue;
        }
        if record.length < 12 {
            return Ok(Some((0, None)));
        }
        let data = record.payload(bytes);
        let offset = u32_at(data, 8) as usize;
        return Ok(Some((u32_at(data, 4), (offset != 0).then_some(offset))));
    }
    Ok(None)
}
fn persist_chain(
    bytes: &[u8],
    current: Option<&[u8]>,
    stop: &Stop<'_>,
) -> Result<(HashMap<u32, usize>, Option<u32>), Error> {
    let mut directory = HashMap::new();
    let Some((0xe391c05f, Some(mut edit_at))) = current
        .map(|current| current_info(current, stop))
        .transpose()?
        .flatten()
    else {
        return Ok((directory, None));
    };
    let mut document_id = None;
    for edits in 0..MAX_CHAIN {
        if (stop.0)() {
            return Err(Error::bare("cancelled"));
        }
        let edit = record_at(bytes, edit_at)
            .filter(|record| record.kind == USER_EDIT && record.length >= 28);
        let Some(edit) = edit else {
            if edits == 0 {
                return Err(Error::new(
                    "bad_persist",
                    format!("no UserEditAtom at {edit_at}"),
                ));
            }
            break;
        };
        let payload = edit.payload(bytes);
        if edits == 0 {
            let id = u32_at(payload, 16);
            document_id = (id != 0).then_some(id);
        }
        let mut at = u32_at(payload, 12) as usize;
        for _ in 0..MAX_CHAIN {
            if (stop.0)() {
                return Err(Error::bare("cancelled"));
            }
            let Some(record) = record_at(bytes, at).filter(|record| record.kind == PERSIST) else {
                break;
            };
            let data = record.payload(bytes);
            let mut cursor = 0;
            while data.len().saturating_sub(cursor) >= 4 {
                let header = u32_at(data, cursor);
                cursor += 4;
                let first = header & 0xfffff;
                let count = (header >> 20) as usize;
                if count == 0 || count > (data.len() - cursor) / 4 {
                    return Err(Error::new(
                        "bad_persist",
                        "a persist directory entry runs off its block",
                    ));
                }
                for index in 0..count {
                    let offset = u32_at(data, cursor) as usize;
                    cursor += 4;
                    if offset < bytes.len() {
                        directory.entry(first + index as u32).or_insert(offset);
                    }
                }
            }
            at = record.end();
        }
        edit_at = u32_at(payload, 8) as usize;
        if edit_at == 0 {
            break;
        }
    }
    Ok((directory, document_id))
}
struct Budget<'a> {
    chars: usize,
    paragraphs: usize,
    cancelled: &'a dyn Fn() -> bool,
}
fn text(bytes: &[u8], record: Record, budget: &mut Budget<'_>) -> Result<String, Error> {
    let payload = record.payload(bytes);
    let charge = if record.kind == TEXT_CHARS {
        payload.len() / 2
    } else {
        payload.len()
    };
    budget.chars = budget.chars.checked_sub(charge).ok_or_else(|| {
        Error::new(
            "limits_exceeded",
            format!("more than {MAX_CHARS} characters of text"),
        )
    })?;
    let decoded: String = if record.kind == TEXT_CHARS {
        char::decode_utf16(
            payload
                .as_chunks::<2>()
                .0
                .iter()
                .map(|part| u16::from_le_bytes(*part)),
        )
        .map(|value| value.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
    } else {
        payload.iter().map(|byte| char::from(*byte)).collect()
    };
    let mut out = String::with_capacity(decoded.len());
    let mut chars = decoded.chars().peekable();
    let mut visited = 0usize;
    while let Some(character) = chars.next() {
        if visited.is_multiple_of(1024) && (budget.cancelled)() {
            return Err(Error::bare("cancelled"));
        }
        visited += 1;
        out.push(match character {
            '\r' => {
                if matches!(chars.peek(), Some('\n' | '\u{b}')) {
                    chars.next();
                }
                '\n'
            }
            '\u{b}' => '\n',
            '\u{1e}' | '\u{1f}' => ' ',
            '\u{91}' => '‘',
            '\u{92}' => '’',
            '\u{93}' => '“',
            '\u{94}' => '”',
            '\u{95}' => '•',
            '\u{96}' => '–',
            '\u{97}' => '—',
            other => other,
        });
    }
    let paragraphs = out.bytes().filter(|byte| *byte == b'\n').count() + 1;
    budget.paragraphs = budget.paragraphs.checked_sub(paragraphs).ok_or_else(|| {
        Error::new(
            "limits_exceeded",
            format!("more than {MAX_PARAGRAPHS} paragraphs of text"),
        )
    })?;
    Ok(out)
}
fn text_kind(bytes: &[u8], record: Record) -> Option<i32> {
    (record.length >= 4).then(|| u32_at(record.payload(bytes), 0) as i32)
}
fn textbox(
    bytes: &[u8],
    record: Record,
    budget: &mut Budget<'_>,
) -> Result<Option<Sequence>, Error> {
    let mut kind = 4;
    let mut value = None;
    for inner in Records::new(bytes, record.offset, record.end(), budget.cancelled) {
        let inner = inner?;
        match inner.kind {
            TEXT_HEADER => kind = text_kind(bytes, inner).unwrap_or(kind),
            TEXT_CHARS | TEXT_BYTES => value = Some(text(bytes, inner, budget)?),
            _ => {}
        }
    }
    Ok(value.map(|text| Sequence { kind, text }))
}
fn collect_boxes(
    bytes: &[u8],
    from: usize,
    end: usize,
    depth: usize,
    budget: &mut Budget<'_>,
    out: &mut Vec<Sequence>,
) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    for record in Records::new(bytes, from, end, budget.cancelled) {
        let record = record?;
        if record.kind == TEXTBOX {
            if let Some(value) = textbox(bytes, record, budget)? {
                out.push(value);
            }
        } else if record.version == 15 {
            collect_boxes(bytes, record.offset, record.end(), depth + 1, budget, out)?;
        }
    }
    Ok(())
}
fn slide(
    bytes: &[u8],
    record: Record,
    outline: Vec<Sequence>,
    budget: &mut Budget<'_>,
) -> Result<Vec<Sequence>, Error> {
    let mut boxes = Vec::new();
    collect_boxes(bytes, record.offset, record.end(), 0, budget, &mut boxes)?;
    let mut out = Vec::new();
    for sequence in outline.into_iter().chain(boxes) {
        if jscompat::trim(&sequence.text).is_empty() {
            continue;
        }
        if out.len() >= MAX_SEQUENCES {
            return Err(Error::new(
                "limits_exceeded",
                format!("more than {MAX_SEQUENCES} text sequences on one slide"),
            ));
        }
        out.push(sequence);
    }
    Ok(out)
}
fn disjoint(records: &[Record]) -> Result<(), Error> {
    let mut sorted = records.to_vec();
    sorted.sort_by_key(|record| record.offset);
    if sorted
        .windows(2)
        .any(|pair| pair[1].offset - 8 < pair[0].end())
    {
        return Err(Error::new("bad_slide", "slide containers overlap"));
    }
    Ok(())
}
enum OutlineTarget {
    Inactive,
    Retained(usize),
    Overflow,
}
pub(super) fn parse(
    bytes: &[u8],
    current: Option<&[u8]>,
    stop: &Stop<'_>,
) -> Result<Presentation, Error> {
    let (directory, document_id) = persist_chain(bytes, current, stop)?;
    let chained = !directory.is_empty();
    let resolve = |id: u32| {
        directory
            .get(&id)
            .and_then(|offset| record_at(bytes, *offset))
    };
    let document = if let Some(id) = document_id {
        Some(
            resolve(id)
                .filter(|record| record.kind == DOCUMENT)
                .ok_or_else(|| {
                    Error::new(
                        "bad_persist",
                        format!("docPersistIdRef {id} is not a DocumentContainer"),
                    )
                })?,
        )
    } else if !chained {
        let mut found = None;
        for record in Records::all(bytes, stop.0) {
            let record = record?;
            if record.kind == DOCUMENT {
                found = Some(record);
                break;
            }
        }
        found
    } else {
        None
    }
    .ok_or_else(|| Error::new("corrupt", "no DocumentContainer"))?;
    let (mut width, mut height) = (5760, 4320);
    for record in Records::new(bytes, document.offset, document.end(), stop.0) {
        let record = record?;
        if record.kind == DOCUMENT_ATOM && record.length >= 8 {
            let data = record.payload(bytes);
            let (x, y) = (u32_at(data, 0), u32_at(data, 4));
            if x > 0 && y > 0 && x <= 1_000_000 && y <= 1_000_000 {
                width = x;
                height = y;
            }
        }
    }
    let mut budget = Budget {
        chars: MAX_CHARS,
        paragraphs: MAX_PARAGRAPHS,
        cancelled: stop.0,
    };
    let mut named = HashSet::new();
    let mut repeats = 0;
    let mut entries: Vec<(u32, Vec<Sequence>)> = Vec::new();
    let mut entry_count = 0;
    for record in Records::new(bytes, document.offset, document.end(), stop.0) {
        let record = record?;
        if record.kind != SLIDE_LIST || record.instance != 0 {
            continue;
        }
        let mut target = OutlineTarget::Inactive;
        let mut kind = None;
        for inner in Records::new(bytes, record.offset, record.end(), stop.0) {
            let inner = inner?;
            match inner.kind {
                SLIDE_PERSIST if inner.length >= 4 => {
                    let id = u32_at(inner.payload(bytes), 0);
                    kind = None;
                    if let Some(offset) = directory.get(&id)
                        && !named.insert(*offset)
                    {
                        repeats += 1;
                        target = OutlineTarget::Inactive;
                        continue;
                    }
                    entry_count += 1;
                    target = if entries.len() <= MAX_SLIDES {
                        entries.push((id, Vec::new()));
                        OutlineTarget::Retained(entries.len() - 1)
                    } else {
                        OutlineTarget::Overflow
                    };
                }
                TEXT_HEADER => kind = text_kind(bytes, inner),
                TEXT_CHARS | TEXT_BYTES if !matches!(target, OutlineTarget::Inactive) => {
                    if let Some(kind_value) = kind.take() {
                        let text = text(bytes, inner, &mut budget)?;
                        if let OutlineTarget::Retained(index) = target {
                            entries[index].1.push(Sequence {
                                kind: kind_value,
                                text,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if entry_count > MAX_SLIDES {
        return Err(Error::new(
            "limits_exceeded",
            format!("more than {MAX_SLIDES} slides"),
        ));
    }
    let mut slides = Vec::new();
    if chained && !entries.is_empty() {
        let records = entries
            .iter()
            .map(|(id, _)| {
                resolve(*id)
                    .filter(|record| record.kind == SLIDE)
                    .ok_or_else(|| Error::new("bad_slide", format!("persist id {id}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        disjoint(&records)?;
        for ((_, outline), record) in entries.into_iter().zip(records) {
            slides.push(slide(bytes, record, outline, &mut budget)?);
        }
    } else {
        let mut containers = Vec::new();
        let mut container_count = 0;
        if chained {
            for offset in directory.values() {
                if (stop.0)() {
                    return Err(Error::bare("cancelled"));
                }
                if let Some(record) =
                    record_at(bytes, *offset).filter(|record| record.kind == SLIDE)
                {
                    if !named.insert(*offset) {
                        repeats += 1;
                        continue;
                    }
                    containers.push(record);
                }
            }
            disjoint(&containers)?;
            container_count = containers.len();
        } else {
            for record in Records::all(bytes, stop.0) {
                let record = record?;
                if record.kind == SLIDE {
                    container_count += 1;
                    if containers.len() <= MAX_SLIDES {
                        containers.push(record);
                    }
                }
            }
        }
        containers.sort_by_key(|record| record.offset);
        if container_count > MAX_SLIDES {
            return Err(Error::new(
                "limits_exceeded",
                format!("more than {MAX_SLIDES} slides"),
            ));
        }
        let use_outline = !chained && entries.len() == containers.len();
        let mut outlines = entries.into_iter();
        for record in containers {
            let outline = if use_outline {
                outlines.next().map(|entry| entry.1).unwrap_or_default()
            } else {
                Vec::new()
            };
            slides.push(slide(bytes, record, outline, &mut budget)?);
        }
    }
    let warnings = if repeats == 0 {
        Vec::new()
    } else {
        vec![format!(
            "{repeats} repeated slide references were skipped; each slide was kept once"
        )]
    };
    Ok(Presentation {
        slides,
        width,
        height,
        warnings,
    })
}
fn stream_name(path: &str) -> &str {
    path.strip_prefix("Root Entry/")
        .unwrap_or_else(|| path.trim_start_matches('/'))
}
pub(super) fn convert(bytes: &[u8], stop: &Stop<'_>) -> Result<Converted, Error> {
    if bytes.is_empty() {
        return Err(Error::new("not_cfb", "the file is empty"));
    }
    if bytes.len() > PPT_MAX_BYTES {
        return Err(Error::new(
            "limits_exceeded",
            format!(
                "the file is {} bytes, over the {PPT_MAX_BYTES}-byte limit",
                bytes.len()
            ),
        ));
    }
    assert_compound_file(bytes).map_err(|failure| match failure {
        Failure::Refused { code, detail } => Error::new(
            match code {
                Ole2FailureCode::NotCfb => "not_cfb",
                Ole2FailureCode::Truncated => "truncated",
                Ole2FailureCode::LimitsExceeded => "limits_exceeded",
                Ole2FailureCode::Corrupt => "corrupt",
            },
            detail,
        ),
        Failure::Range => Error::new(
            "corrupt",
            "the OLE2 container cannot be read (Offset is outside the bounds of the DataView)",
        ),
    })?;
    if (stop.0)() {
        return Err(Error::bare("cancelled"));
    }
    let entries =
        read_index_with_stream_limit(bytes, PPT_MAX_STREAM_BYTES).map_err(
            |failure| match failure {
                IndexReadError::Read(message) => Error::new(
                    "corrupt",
                    format!("the OLE2 container cannot be read ({message})"),
                ),
                IndexReadError::StreamLimit { full_path, size } => Error::new(
                    "limits_exceeded",
                    format!(
                        "the {} stream is {size} bytes, over the {PPT_MAX_STREAM_BYTES}-byte limit",
                        stream_name(&full_path)
                    ),
                ),
            },
        )?;
    let mut streams = HashMap::new();
    for entry in entries {
        if entry.kind != Some(2) {
            continue;
        }
        let relative = stream_name(&entry.full_path);
        let name = if relative.is_empty() {
            entry.name.clone()
        } else {
            relative.to_owned()
        };
        if (stop.0)() {
            return Err(Error::bare("cancelled"));
        }
        if name.is_empty() || name.starts_with('\u{1}') {
            continue;
        }
        streams
            .entry(name)
            .or_insert_with(|| entry.content.unwrap_or_default());
    }
    let document = streams.get("PowerPoint Document").ok_or_else(|| {
        Error::new(
            "missing_stream",
            "the OLE2 container has no PowerPoint Document stream",
        )
    })?;
    let current = streams.get("Current User").map(Vec::as_slice);
    if current
        .map(|current| current_info(current, stop))
        .transpose()?
        .flatten()
        .is_some_and(|info| info.0 == 0xf3d1c4df)
    {
        return Err(Error::bare("encrypted"));
    }
    let presentation = parse(document, current, stop)?;
    let output = super::ppt_writer::write(&presentation, stop).map_err(|error| {
        if error == "conversion cancelled" {
            Error::bare("cancelled")
        } else {
            Error::new("corrupt", error)
        }
    })?;
    if (stop.0)() {
        return Err(Error::bare("cancelled"));
    }
    let mut warnings = presentation.warnings;
    warnings.push(
        "shapes land as stacked text boxes; the original geometry is a later stage".to_owned(),
    );
    warnings.push("text formatting (runs, indents, autofit) is a later stage".to_owned());
    Ok(Converted {
        bytes: output,
        slides: presentation.slides.len(),
        warnings,
        dropped: Vec::new(),
    })
}
