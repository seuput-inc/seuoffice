//! Byte and object parsing for pdf-lib's tolerant PDF reader. This model is
//! deliberately separate from the xref-driven diagnostic parser.
mod assembly;
pub(super) use assembly::assemble;
#[cfg(test)]
pub(super) use assembly::tests::classic as synthetic_classic_pdf;
mod graph;
mod revision;
pub(in crate::host::d2::pdf) mod stream;
mod upng;

use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub(in crate::host::d2::pdf) enum Object {
    Null,
    Bool(bool),
    Number(f64),
    Name(Vec<u8>),
    String(Vec<u8>),
    HexString(Vec<u8>),
    Array(Vec<Object>),
    Dictionary(Vec<(Vec<u8>, Object)>),
    Reference(f64, f64),
    Stream(Vec<(Vec<u8>, Object)>, Vec<u8>),
    Invalid(Vec<u8>),
}

pub(in crate::host::d2::pdf) struct Parser<'a> {
    pub(in crate::host::d2::pdf) bytes: &'a [u8],
    pub(in crate::host::d2::pdf) offset: usize,
}

fn whitespace(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}
fn delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}
fn numeric(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')
}

impl<'a> Parser<'a> {
    pub(in crate::host::d2::pdf) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    pub(in crate::host::d2::pdf) fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }
    pub(in crate::host::d2::pdf) fn done(&self) -> bool {
        self.offset >= self.bytes.len()
    }
    pub(in crate::host::d2::pdf) fn keyword(&mut self, keyword: &[u8]) -> bool {
        if self
            .bytes
            .get(self.offset..)
            .is_some_and(|tail| tail.starts_with(keyword))
        {
            self.offset += keyword.len();
            true
        } else {
            false
        }
    }
    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(whitespace) {
            self.offset += 1;
        }
    }
    pub(in crate::host::d2::pdf) fn skip_space(&mut self) {
        loop {
            while self.peek().is_some_and(whitespace) {
                self.offset += 1;
            }
            if self.peek() != Some(b'%') {
                return;
            }
            while self
                .peek()
                .is_some_and(|byte| byte != b'\r' && byte != b'\n')
            {
                self.offset += 1;
            }
        }
    }
    pub(in crate::host::d2::pdf) fn integer(&mut self) -> Result<f64, String> {
        let start = self.offset;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.offset += 1;
        }
        self.number_value(start)
    }
    fn number_value(&self, start: usize) -> Result<f64, String> {
        if start == self.offset {
            return Err("invalid PDF number".into());
        }
        let text = std::str::from_utf8(&self.bytes[start..self.offset])
            .map_err(|_| "invalid PDF number")?;
        let number = jscompat::to_number(text);
        if number.is_finite() {
            Ok(number)
        } else {
            Err("invalid PDF number".into())
        }
    }
    fn number(&mut self) -> Result<f64, String> {
        let start = self.offset;
        while self.peek().is_some_and(numeric) {
            let byte = self.peek().unwrap_or_default();
            self.offset += 1;
            if byte == b'.' {
                break;
            }
        }
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.offset += 1;
        }
        self.number_value(start)
    }
    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() != Some(byte) {
            return Err("invalid PDF object".into());
        }
        self.offset += 1;
        Ok(())
    }
    fn name(&mut self) -> Result<Vec<u8>, String> {
        self.expect(b'/')?;
        let start = self.offset;
        while self
            .peek()
            .is_some_and(|byte| !whitespace(byte) && !delimiter(byte))
        {
            self.offset += 1;
        }
        let raw = &self.bytes[start..self.offset];
        let mut name = Vec::with_capacity(raw.len());
        let mut at = 0;
        while at < raw.len() {
            if raw[at] == b'#'
                && at + 2 < raw.len()
                && raw[at + 1].is_ascii_hexdigit()
                && !raw[at + 1].is_ascii_lowercase()
                && raw[at + 2].is_ascii_hexdigit()
                && !raw[at + 2].is_ascii_lowercase()
            {
                let digit = |value: u8| {
                    if value.is_ascii_digit() {
                        value - b'0'
                    } else {
                        value - b'A' + 10
                    }
                };
                name.push((digit(raw[at + 1]) << 4) | digit(raw[at + 2]));
                at += 3;
            } else {
                name.push(raw[at]);
                at += 1;
            }
        }
        Ok(name)
    }
    fn literal(&mut self) -> Result<Vec<u8>, String> {
        self.expect(b'(')?;
        let start = self.offset;
        let mut nesting = 1usize;
        let mut escaped = false;
        while let Some(byte) = self.peek() {
            self.offset += 1;
            if !escaped {
                if byte == b'(' {
                    nesting += 1;
                }
                if byte == b')' {
                    nesting -= 1;
                }
            }
            if byte == b'\\' {
                escaped = !escaped;
            } else {
                escaped = false;
            }
            if nesting == 0 {
                return Ok(self.bytes[start..self.offset - 1].to_vec());
            }
        }
        Err("unbalanced PDF string".into())
    }
    fn hex_string(&mut self) -> Result<Vec<u8>, String> {
        self.expect(b'<')?;
        let start = self.offset;
        while self.peek().is_some_and(|byte| byte != b'>') {
            self.offset += 1;
        }
        let end = self.offset;
        self.expect(b'>')?;
        Ok(self.bytes[start..end].to_vec())
    }
    fn array(&mut self) -> Result<Vec<Object>, String> {
        self.expect(b'[')?;
        self.skip_space();
        let mut items = Vec::new();
        while self.peek() != Some(b']') {
            if self.done() {
                return Err("unterminated PDF array".into());
            }
            items.push(self.object()?);
            self.skip_space();
        }
        self.expect(b']')?;
        Ok(items)
    }
    fn dictionary(&mut self) -> Result<Vec<(Vec<u8>, Object)>, String> {
        self.expect(b'<')?;
        self.expect(b'<')?;
        self.skip_space();
        let mut entries: Vec<(Vec<u8>, Object)> = Vec::new();
        while !self.done()
            && self.peek() != Some(b'>')
            && self.bytes.get(self.offset + 1) != Some(&b'>')
        {
            let key = self.name()?;
            let value = self.object()?;
            if let Some((_, previous)) = entries.iter_mut().find(|(name, _)| *name == key) {
                *previous = value;
            } else {
                entries.push((key, value));
            }
            self.skip_space();
        }
        self.skip_space();
        self.expect(b'>')?;
        self.expect(b'>')?;
        Ok(entries)
    }
    fn stream_end(&mut self) -> Result<usize, String> {
        let mut nesting = 1usize;
        while !self.done() {
            let end = self.offset;
            if self.keyword(b"stream") {
                nesting += 1;
            } else if self.keyword(b"\r\nendstream")
                || self.keyword(b"\rendstream")
                || self.keyword(b"\nendstream")
                || self.keyword(b"endstream")
            {
                nesting -= 1;
            } else {
                self.offset += 1;
            }
            if nesting == 0 {
                return Ok(end);
            }
        }
        Err("unterminated PDF stream".into())
    }
    fn dictionary_or_stream(&mut self) -> Result<Object, String> {
        let dict = self.dictionary()?;
        self.skip_space();
        if !self.keyword(b"stream \r\n")
            && !self.keyword(b"stream\r\n")
            && !self.keyword(b"stream\r")
            && !self.keyword(b"stream\n")
            && !self.keyword(b"stream")
        {
            return Ok(Object::Dictionary(dict));
        }
        let start = self.offset;
        let declared = dict
            .iter()
            .find(|(key, _)| key == b"Length")
            .map(|(_, value)| value);
        let end = if let Some(Object::Number(length)) = declared {
            if *length >= 0.0 && length.fract() == 0.0 && *length <= usize::MAX as f64 {
                if let Some(end) = start
                    .checked_add(*length as usize)
                    .filter(|end| *end <= self.bytes.len())
                {
                    self.offset = end;
                    self.skip_space();
                    if self.keyword(b"endstream") {
                        end
                    } else {
                        self.offset = start;
                        self.stream_end()?
                    }
                } else {
                    self.offset = start;
                    self.stream_end()?
                }
            } else {
                self.offset = start;
                self.stream_end()?
            }
        } else {
            self.stream_end()?
        };
        Ok(Object::Stream(dict, self.bytes[start..end].to_vec()))
    }
    pub(in crate::host::d2::pdf) fn object(&mut self) -> Result<Object, String> {
        self.skip_space();
        if self.keyword(b"true") {
            return Ok(Object::Bool(true));
        }
        if self.keyword(b"false") {
            return Ok(Object::Bool(false));
        }
        if self.keyword(b"null") {
            return Ok(Object::Null);
        }
        match self.peek() {
            Some(b'<') if self.bytes.get(self.offset + 1) == Some(&b'<') => {
                self.dictionary_or_stream()
            }
            Some(b'<') => self.hex_string().map(Object::HexString),
            Some(b'(') => self.literal().map(Object::String),
            Some(b'/') => self.name().map(Object::Name),
            Some(b'[') => self.array().map(Object::Array),
            Some(byte) if numeric(byte) => {
                let first = self.number()?;
                self.skip_space();
                let lookahead = self.offset;
                if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                    let second = self.number()?;
                    self.skip_space();
                    if self.keyword(b"R") {
                        return Ok(Object::Reference(first, second));
                    }
                }
                self.offset = lookahead;
                Ok(Object::Number(first))
            }
            _ => Err("invalid PDF object".into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(in crate::host::d2::pdf) struct RefKey(
    pub(in crate::host::d2::pdf) u64,
    pub(in crate::host::d2::pdf) u64,
);

impl RefKey {
    pub(in crate::host::d2::pdf) fn new(object: f64, generation: f64) -> Self {
        let bits = |value: f64| if value == 0.0 { 0 } else { value.to_bits() };
        Self(bits(object), bits(generation))
    }
}

#[derive(Default)]
pub(in crate::host::d2::pdf) struct Context {
    pub(in crate::host::d2::pdf) objects: HashMap<RefKey, Object>,
    pub(in crate::host::d2::pdf) order: Vec<RefKey>,
    pub(in crate::host::d2::pdf) root: Option<Object>,
    pub(in crate::host::d2::pdf) encrypt: Option<Object>,
    pub(in crate::host::d2::pdf) info: Option<Object>,
    pub(in crate::host::d2::pdf) id: Option<Object>,
    pub(in crate::host::d2::pdf) largest_object_number: f64,
}

fn field<'a>(dict: &'a [(Vec<u8>, Object)], key: &[u8]) -> Option<&'a Object> {
    dict.iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

impl Context {
    pub(in crate::host::d2::pdf) fn assign(&mut self, object: f64, generation: f64, value: Object) {
        let key = RefKey::new(object, generation);
        if !self.objects.contains_key(&key) {
            self.order.push(key);
        }
        self.objects.insert(key, value);
        if object > self.largest_object_number {
            self.largest_object_number = object;
        }
    }

    pub(in crate::host::d2::pdf) fn lookup<'a>(&'a self, object: &'a Object) -> Option<&'a Object> {
        match object {
            Object::Reference(number, generation) => {
                self.objects.get(&RefKey::new(*number, *generation))
            }
            value => Some(value),
        }
    }

    fn lookup_field<'a>(
        &'a self,
        dict: &'a [(Vec<u8>, Object)],
        name: &[u8],
    ) -> Option<&'a Object> {
        field(dict, name).and_then(|value| self.lookup(value))
    }

    fn number(&self, dict: &[(Vec<u8>, Object)], name: &[u8]) -> Result<f64, String> {
        match self.lookup_field(dict, name) {
            Some(Object::Number(value)) => Ok(*value),
            _ => Err("invalid PDF stream dictionary".into()),
        }
    }

    fn recover_root(&mut self) {
        let is_catalog = |object: &Object| {
            if let Some(Object::Dictionary(dict)) = self.lookup(object) {
                matches!(self.lookup_field(dict, b"Type"), Some(Object::Name(name)) if name == b"Catalog")
            } else {
                false
            }
        };
        if self.root.as_ref().is_some_and(is_catalog) {
            return;
        }
        let mut refs = self.order.clone();
        refs.sort_by(|a, b| f64::from_bits(a.0).total_cmp(&f64::from_bits(b.0)));
        let found = refs
            .into_iter()
            .filter(|key| self.objects.get(key).is_some_and(is_catalog))
            .last();
        if let Some(key) = found {
            self.root = Some(Object::Reference(
                f64::from_bits(key.0),
                f64::from_bits(key.1),
            ));
        }
    }

    fn object_stream(&mut self, dict: &[(Vec<u8>, Object)], bytes: &[u8]) -> Result<(), String> {
        let decoded = stream::decode(self, dict, bytes)?;
        let first = self.number(dict, b"First")?;
        let count = self.number(dict, b"N")?;
        let mut parser = Parser::new(&decoded);
        let mut offsets = Vec::new();
        let mut index = 0usize;
        while (index as f64) < count {
            parser.skip_space();
            let number = parser.integer()?;
            parser.skip_space();
            let offset = parser.integer()?;
            offsets.push((number, offset));
            index += 1;
        }
        for (number, offset) in offsets {
            let position = first + offset;
            if position < 0.0 || position.fract() != 0.0 || position > usize::MAX as f64 {
                return Err("invalid PDF object stream offset".into());
            }
            parser.offset = position as usize;
            let object = parser.object()?;
            self.assign(number, 0.0, object);
        }
        Ok(())
    }

    fn xref_stream(&mut self, dict: &[(Vec<u8>, Object)], bytes: &[u8]) -> Result<(), String> {
        let _decoded = stream::decode(self, dict, bytes)?;
        self.number(dict, b"Size")?;
        if let Some(Object::Array(index)) = self.lookup_field(dict, b"Index") {
            for pair in index.chunks(2) {
                if pair.len() != 2
                    || pair
                        .iter()
                        .any(|value| !matches!(self.lookup(value), Some(Object::Number(_))))
                {
                    return Err("invalid PDF xref stream index".into());
                }
            }
        }
        let Some(Object::Array(widths)) = self.lookup_field(dict, b"W") else {
            return Err("invalid PDF xref stream widths".into());
        };
        if widths
            .iter()
            .any(|value| !matches!(self.lookup(value), Some(Object::Number(_))))
        {
            return Err("invalid PDF xref stream widths".into());
        }
        self.root = field(dict, b"Root").cloned();
        self.encrypt = field(dict, b"Encrypt").cloned();
        self.info = field(dict, b"Info").cloned();
        self.id = field(dict, b"ID").cloned();
        Ok(())
    }
}

impl Parser<'_> {
    fn indirect_header(&mut self) -> Result<(f64, f64), String> {
        self.skip_space();
        let number = self.integer()?;
        self.skip_space();
        let generation = self.integer()?;
        self.skip_space();
        if !self.keyword(b"obj") {
            return Err("invalid PDF indirect header".into());
        }
        Ok((number, generation))
    }

    fn is_indirect_header(&mut self) -> bool {
        let start = self.offset;
        let found = self.indirect_header().is_ok();
        self.offset = start;
        found
    }

    fn skip_jibberish(&mut self) {
        self.skip_space();
        while let Some(byte) = self.peek() {
            let start = self.offset;
            if (b' '..=b'~').contains(&byte)
                && (self.keyword(b"xref")
                    || self.keyword(b"trailer")
                    || self.keyword(b"startxref")
                    || self.is_indirect_header())
            {
                self.offset = start;
                return;
            }
            self.offset = start + 1;
        }
    }

    fn indirect(&mut self, context: &mut Context) -> Result<(), String> {
        let (number, generation) = self.indirect_header()?;
        self.skip_space();
        let object = self.object()?;
        self.skip_space();
        self.keyword(b"endobj");
        if let Object::Stream(dict, bytes) = &object {
            match context.lookup_field(dict, b"Type") {
                Some(Object::Name(name)) if name == b"ObjStm" => {
                    return context.object_stream(dict, bytes);
                }
                Some(Object::Name(name)) if name == b"XRef" => {
                    return context.xref_stream(dict, bytes);
                }
                _ => {}
            }
        }
        context.assign(number, generation, object);
        Ok(())
    }

    fn invalid_indirect(&mut self, context: &mut Context) -> Result<(), String> {
        let (number, generation) = self.indirect_header()?;
        self.skip_space();
        let start = self.offset;
        while !self.done() {
            if self.keyword(b"endobj") {
                let end = self.offset - b"endobj".len();
                context.assign(
                    number,
                    generation,
                    Object::Invalid(self.bytes[start..end].to_vec()),
                );
                return Ok(());
            }
            self.offset += 1;
        }
        Err("invalid PDF object has no endobj".into())
    }

    fn section(&mut self, context: &mut Context) -> Result<(), String> {
        self.skip_space();
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            let start = self.offset;
            if self.indirect(context).is_err() {
                self.offset = start;
                self.invalid_indirect(context)?;
            }
            self.skip_space();
            self.skip_jibberish();
        }
        self.skip_space();
        if self.keyword(b"xref") {
            self.skip_space();
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.integer()?;
                self.skip_space();
                self.integer()?;
                self.skip_space();
                if matches!(self.peek(), Some(b'n' | b'f')) {
                    self.offset += 1;
                }
                self.skip_space();
            }
        }
        self.skip_space();
        if self.keyword(b"trailer") {
            self.skip_space();
            let dict = self.dictionary()?;
            if let Some(value) = field(&dict, b"Root") {
                context.root = Some(value.clone());
            }
            if let Some(value) = field(&dict, b"Encrypt") {
                context.encrypt = Some(value.clone());
            }
            if let Some(value) = field(&dict, b"Info") {
                context.info = Some(value.clone());
            }
            if let Some(value) = field(&dict, b"ID") {
                context.id = Some(value.clone());
            }
        }
        self.skip_space();
        if self.keyword(b"startxref") {
            self.skip_space();
            self.integer()?;
            self.skip_whitespace();
            self.keyword(b"%%EOF");
            self.skip_space();
            self.keyword(b"%%EOF");
            self.skip_space();
        }
        self.skip_jibberish();
        Ok(())
    }

    pub(in crate::host::d2::pdf) fn document(&mut self) -> Result<Context, String> {
        while !self.done() && !self.keyword(b"%PDF-") {
            self.offset += 1;
        }
        if self.done() {
            return Err("missing PDF header".into());
        }
        self.integer()?;
        self.expect(b'.')?;
        self.integer()?;
        self.skip_space();
        if !self.is_indirect_header() && !self.done() {
            self.offset += 1;
            self.skip_space();
        }
        let mut context = Context::default();
        let mut previous = None;
        while !self.done() {
            self.section(&mut context)?;
            if previous == Some(self.offset) {
                return Err("stalled PDF parser".into());
            }
            previous = Some(self.offset);
        }
        context.recover_root();
        let invalid_zero = RefKey::new(0.0, 0.0);
        context.objects.remove(&invalid_zero);
        context.order.retain(|key| *key != invalid_zero);
        Ok(context)
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct PreviousXref {
    pub(super) offset: usize,
    pub(super) is_stream: bool,
    pub(super) size: f64,
}

/// Mirror the assembler's tail-based startxref and Size sniffing. This is
/// distinct from parsing the xref itself: pdf-lib assigns objects sequentially.
pub(super) fn previous_xref(bytes: &[u8]) -> Result<PreviousXref, String> {
    fn space(byte: u8) -> bool {
        matches!(byte, 9..=13 | 32 | 0xa0)
    }
    fn integer(bytes: &[u8], mut at: usize) -> Option<(f64, usize)> {
        let start = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == start {
            return None;
        }
        let value = std::str::from_utf8(&bytes[start..at])
            .ok()?
            .parse::<f64>()
            .ok()?;
        Some((value, at))
    }
    let tail = &bytes[bytes.len().saturating_sub(2048)..];
    let marker = tail
        .windows(9)
        .rposition(|part| part == b"startxref")
        .ok_or("missing startxref")?;
    let mut at = marker + 9;
    let start = at;
    while tail.get(at).is_some_and(|byte| space(*byte)) {
        at += 1;
    }
    if at == start {
        return Err("invalid startxref".into());
    }
    let (offset, _) = integer(tail, at).ok_or("invalid startxref")?;
    if !offset.is_finite()
        || offset > 9_007_199_254_740_991.0
        || offset <= 0.0
        || offset >= bytes.len() as f64
    {
        return Err("invalid startxref offset".into());
    }
    let offset = offset as usize;
    let head = &bytes[offset..bytes.len().min(offset.saturating_add(4096))];
    let trimmed = head
        .iter()
        .position(|byte| !space(*byte))
        .unwrap_or(head.len());
    let is_stream = !head[trimmed..].starts_with(b"xref");
    let search_from = if is_stream {
        0
    } else {
        head.windows(7)
            .position(|part| part == b"trailer")
            .unwrap_or(0)
    };
    let mut size = 0.0;
    let search = &head[search_from..];
    for index in 0..search.len().saturating_sub(4) {
        if &search[index..index + 5] != b"/Size" {
            continue;
        }
        let mut next = index + 5;
        let start = next;
        while search.get(next).is_some_and(|byte| space(*byte)) {
            next += 1;
        }
        if next == start {
            continue;
        }
        if let Some((value, _)) = integer(search, next) {
            size = value;
            break;
        }
    }
    Ok(PreviousXref {
        offset,
        is_stream,
        size,
    })
}

/// Structural half of the request-bound signing proof. This deliberately does
/// not sign or issue a capability: parser/decoder differential proof and CMS
/// retry verification remain prerequisites to the caller enabling signing.
pub(super) fn validate_revision(
    original: &[u8],
    revised: &[u8],
    options: &shell_contract::pdf_sign_options::SignOptions,
    signing_time: &str,
    range: [usize; 4],
) -> Result<(), String> {
    let previous = previous_xref(original)?;
    let new_xref = previous_xref(revised)?;
    let original_context = Parser::new(original).document()?;
    let revised_context = Parser::new(revised).document()?;
    let largest = original_context
        .largest_object_number
        .max(previous.size - 1.0);
    if !largest.is_finite()
        || largest < 0.0
        || largest.fract() != 0.0
        || largest > 9_007_199_254_740_980.0
    {
        return Err("invalid next PDF object number".into());
    }
    let signature = largest as u64 + 1;
    let objects = revision::scan(
        original,
        revised,
        new_xref.offset,
        signature,
        options,
        signing_time,
        range,
    )?;
    let expected = graph::validate_graph(
        &original_context,
        &revised_context,
        previous.size,
        options,
        signing_time,
        range,
    )?;
    let actual: std::collections::HashSet<RefKey> = objects
        .iter()
        .map(|object| RefKey::new(object.number as f64, object.generation as f64))
        .collect();
    if actual != expected {
        return Err("physical PDF object set differs from native operation".into());
    }
    let last = objects
        .iter()
        .map(|object| object.number)
        .max()
        .unwrap_or(0)
        .max(largest as u64);
    let size = last.checked_add(1).ok_or("PDF xref size overflow")?;
    if previous.is_stream {
        revision::validate_xref_stream(
            revised,
            new_xref.offset,
            &original_context,
            previous.offset,
            size,
            &objects,
        )?;
    } else {
        revision::validate_xref_table(
            revised,
            new_xref.offset,
            &original_context,
            previous.offset,
            size,
            &objects,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_last_xref_and_size_like_the_assembler() {
        let table =
            b"%PDF\nxref\n0 1\n0000000000 65535 f\ntrailer\n<< /Size 42 >>\nstartxref\n5\n%%EOF\n";
        assert_eq!(
            previous_xref(table).unwrap(),
            PreviousXref {
                offset: 5,
                is_stream: false,
                size: 42.0
            }
        );
        let stream = b"%PDF\n1 0 obj << /Type /XRef /Size 9 >> stream\nx\nendstream endobj\nstartxref\n5\n%%EOF\n";
        assert_eq!(
            previous_xref(stream).unwrap(),
            PreviousXref {
                offset: 5,
                is_stream: true,
                size: 9.0
            }
        );
        assert!(previous_xref(b"%PDF\nstartxref\n999\n%%EOF").is_err());
        assert!(previous_xref(b"%PDF\nstartxref\n5\n%%EOF\nstartxref broken").is_err());
    }

    #[test]
    fn parses_duplicate_names_and_preserves_literal_escapes() {
        let mut parser = Parser::new(br"<< /A#42 (one\(two\)) /AB [1 2 R true] >>");
        assert_eq!(
            parser.object().unwrap(),
            Object::Dictionary(vec![(
                b"AB".to_vec(),
                Object::Array(vec![Object::Reference(1.0, 2.0), Object::Bool(true)])
            )])
        );
        let mut parser = Parser::new(br"(one\(two\))");
        assert_eq!(
            parser.object().unwrap(),
            Object::String(br"one\(two\)".to_vec())
        );
    }

    #[test]
    fn falls_back_to_nested_stream_end_when_length_is_wrong() {
        let mut parser =
            Parser::new(b"<< /Length 1 >>\nstream\nabc stream inner endstream tail\nendstream");
        assert_eq!(
            parser.object().unwrap(),
            Object::Stream(
                vec![(b"Length".to_vec(), Object::Number(1.0))],
                b"abc stream inner endstream tail".to_vec()
            )
        );
    }

    #[test]
    fn sequential_redefinitions_override_xref_and_invalid_objects_are_kept() {
        let input = b"%PDF-1.7\n1 0 obj << /Type /Catalog /Marker /Old >> endobj\n1 0 obj << /Type /Catalog /Marker /New >> endobj\n3 0 obj ??? endobj\ntrailer << /Root 1 0 R >>\nstartxref\n0\n%%EOF\n";
        let context = Parser::new(input).document().unwrap();
        let catalog = context.lookup(&Object::Reference(1.0, 0.0)).unwrap();
        let Object::Dictionary(dict) = catalog else {
            panic!("expected catalog")
        };
        assert_eq!(field(dict, b"Marker"), Some(&Object::Name(b"New".to_vec())));
        assert_eq!(
            context.lookup(&Object::Reference(3.0, 0.0)),
            Some(&Object::Invalid(b"??? ".to_vec()))
        );
    }

    #[test]
    fn object_stream_assigns_children_without_assigning_container() {
        let input = b"%PDF-1.7\n1 0 obj << /Type /ObjStm /N 1 /First 4 >>\nstream\n2 0 << /Type /Catalog >>\nendstream\nendobj\ntrailer << /Root 2 0 R >>\nstartxref\n0\n%%EOF\n";
        let context = Parser::new(input).document().unwrap();
        assert_eq!(context.lookup(&Object::Reference(1.0, 0.0)), None);
        assert!(matches!(
            context.lookup(&Object::Reference(2.0, 0.0)),
            Some(Object::Dictionary(_))
        ));
    }

    #[test]
    fn compressed_object_stream_uses_context_and_assigns_in_order() {
        let payload = b"2 0 2 4 111 222";
        let length = payload.len() as u16;
        let mut compressed = vec![0x78, 0x01, 0x01];
        compressed.extend_from_slice(&length.to_le_bytes());
        compressed.extend_from_slice(&(!length).to_le_bytes());
        compressed.extend_from_slice(payload);
        let dict = vec![
            (b"Filter".to_vec(), Object::Name(b"FlateDecode".to_vec())),
            (b"N".to_vec(), Object::Number(2.0)),
            (b"First".to_vec(), Object::Number(8.0)),
        ];
        let mut context = Context::default();
        context.object_stream(&dict, &compressed).unwrap();
        assert_eq!(
            context.lookup(&Object::Reference(2.0, 0.0)),
            Some(&Object::Number(222.0))
        );
        assert_eq!(context.order.len(), 1);
    }
}
