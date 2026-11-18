use ttf_parser::{Face, GlyphId, Tag, gpos::{PairAdjustment, PositioningSubtable}};

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at.checked_add(2)?)?.try_into().ok()?))
}
fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

/// Match opentype.js 2's default DFLT/latn `kern` feature and first matched
/// pair; unlike a full text shaper it does not apply unrelated GPOS lookups.
pub(super) fn pair(face: &Face<'_>, left: GlyphId, right: GlyphId) -> i16 {
    if let Some(gpos) = face.tables().gpos {
        let script = gpos.scripts.find(Tag::from_bytes(b"DFLT")).or_else(|| gpos.scripts.find(Tag::from_bytes(b"latn")));
        let Some(language) = script.and_then(|script| script.default_language) else { return 0 };
        let Some(feature) = language.feature_indices.into_iter().filter_map(|index| gpos.features.get(index)).find(|feature| feature.tag == Tag::from_bytes(b"kern")) else { return 0 };
        let raw = face.raw_face().table(Tag::from_bytes(b"GPOS"));
        for index in feature.lookup_indices {
            // opentype.js only admits lookup type 2, not extension type 9.
            let kind = raw.and_then(|bytes| {
                let list = usize::from(u16_at(bytes, 8)?);
                let offset = usize::from(u16_at(bytes, list + 2 + usize::from(index) * 2)?);
                u16_at(bytes, list + offset)
            });
            if kind != Some(2) { continue; }
            let Some(lookup) = gpos.lookups.get(index) else { continue };
            for subtable in lookup.subtables.into_iter::<PositioningSubtable<'_>>() {
                let PositioningSubtable::Pair(pair) = subtable else { continue };
                match pair {
                    PairAdjustment::Format1 { coverage, sets } => {
                        if let Some(value) = coverage.get(left).and_then(|index| sets.get(index)).and_then(|set| set.get(right)) {
                            return value.0.x_advance;
                        }
                    }
                    PairAdjustment::Format2 { coverage, classes, matrix } => {
                        if coverage.contains(left) {
                            return matrix.get((classes.0.get(left), classes.1.get(right))).map_or(0, |value| value.0.x_advance);
                        }
                    }
                }
            }
        }
        return 0;
    }
    let Some(raw) = face.raw_face().table(Tag::from_bytes(b"kern")) else { return 0 };
    let Some(version) = u16_at(raw, 0) else { return 0 };
    let start = match version {
        // Windows: version, nTables, version, length, coverage, nPairs.
        0 if u16_at(raw, 4) == Some(0) => 10,
        // Apple: fixed version, nTables, length, coverage, tuple index, nPairs.
        1 if u32_at(raw, 4).is_some() && u16_at(raw, 12).is_some_and(|coverage| coverage & 0xff == 0) => 16,
        _ => return 0,
    };
    let Some(count) = u16_at(raw, start) else { return 0 };
    let mut found = 0;
    for index in 0..usize::from(count) {
        let at = start + 8 + index * 6;
        let (Some(l), Some(r), Some(value)) = (u16_at(raw, at), u16_at(raw, at + 2), u16_at(raw, at + 4)) else { break };
        if l == left.0 && r == right.0 { found = value as i16; }
    }
    found
}
