use epub_engine::blocks::{self, ChapterMap as NativeMap, OpenChapter};
use epub_engine::edits::{self, BlockEdit, EditsResult, MarkKind, Neighbour};
use epub_engine::ranges::RangeReplacement;
use epub_engine::xml_positions::PositionsHost;
use serde_json::{Value, json};
use shell_ipc_d2::epub::hosts::{Block, BlockEdits, ChapterMap, Replacement};

fn native_replacements(items: &[Replacement]) -> Vec<RangeReplacement> {
    items
        .iter()
        .map(|item| RangeReplacement {
            start: item.start as f64,
            end: item.end as f64,
            text: item.text.encode_utf16().collect(),
        })
        .collect()
}

fn visible(map: NativeMap) -> ChapterMap {
    match map {
        NativeMap::Broken(map) => ChapterMap {
            source: String::from_utf16_lossy(&map.source),
            broken: true,
            error: Some(map.error),
            blocks: Vec::new(),
            next_id: 1,
        },
        NativeMap::Open(map) => ChapterMap {
            source: String::from_utf16_lossy(map.source()),
            broken: false,
            error: None,
            blocks: map
                .blocks
                .iter()
                .map(|block| Block {
                    id: block.id.clone(),
                    kind: block.kind.as_str().to_owned(),
                    tag: block.tag.clone(),
                    flowing: block.flowing,
                    outer: (block.outer.start, block.outer.end),
                    inner: (block.inner.start, block.inner.end),
                })
                .collect(),
            next_id: map.next_id as u64,
        },
    }
}

pub(super) fn open(source: &str) -> Result<ChapterMap, String> {
    blocks::open_chapter(&PositionsHost, &source.encode_utf16().collect::<Vec<_>>())
        .map(visible)
        .map_err(|error| error.message)
}

fn restore(map: &ChapterMap) -> Result<OpenChapter, String> {
    let source: Vec<_> = map.source.encode_utf16().collect();
    let native = blocks::open_chapter(&PositionsHost, &source).map_err(|error| error.message)?;
    let NativeMap::Open(mut native) = native else {
        return Err("chapter is not well formed".into());
    };
    if native.blocks.len() != map.blocks.len() {
        return Err("chapter block map does not match its source".into());
    }
    for (native, supplied) in native.blocks.iter_mut().zip(&map.blocks) {
        if (native.outer.start, native.outer.end) != supplied.outer
            || (native.inner.start, native.inner.end) != supplied.inner
        {
            return Err("chapter block map does not match its source".into());
        }
        native.id.clone_from(&supplied.id);
    }
    native.next_id = usize::try_from(map.next_id).map_err(|_| "invalid next block id")?;
    Ok(native)
}

fn edit(value: &Value) -> Result<BlockEdit, String> {
    let string = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("missing {key}"))
    };
    let number = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("missing {key}"))
    };
    let id: Vec<u16> = string("blockId")?.encode_utf16().collect();
    Ok(match string("op")? {
        "replace" => BlockEdit::Replace {
            block_id: id,
            text: string("text")?.encode_utf16().collect(),
            drop_ruby: value.get("dropRuby").and_then(Value::as_bool) == Some(true),
        },
        "insert_after" => BlockEdit::InsertAfter {
            block_id: id,
            text: string("text")?.encode_utf16().collect(),
        },
        "delete" => BlockEdit::Delete { block_id: id },
        "split" => BlockEdit::Split {
            block_id: id,
            offset: number("offset")?,
        },
        "line_break" => BlockEdit::LineBreak {
            block_id: id,
            offset: number("offset")?,
        },
        "merge" => BlockEdit::Merge {
            block_id: id,
            with: match string("with")? {
                "previous" => Neighbour::Previous,
                "next" => Neighbour::Next,
                _ => return Err("invalid merge direction".into()),
            },
        },
        "toggle_mark" => BlockEdit::ToggleMark {
            block_id: id,
            mark: match string("mark")? {
                "bold" => MarkKind::Bold,
                "italic" => MarkKind::Italic,
                _ => return Err("invalid mark".into()),
            },
            start: number("start")?,
            end: number("end")?,
        },
        _ => return Err("invalid block edit".into()),
    })
}

pub(super) fn apply(map: &ChapterMap, edits: &[Value]) -> Result<BlockEdits, String> {
    let native = restore(map)?;
    let edits: Vec<_> = edits.iter().map(edit).collect::<Result<_, _>>()?;
    match edits::apply_block_edits(&PositionsHost, &native, &edits)
        .map_err(|error| error.message)?
    {
        EditsResult::Applied {
            map,
            replacements,
            created,
        } => Ok(BlockEdits::Applied {
            map: visible(NativeMap::Open(map)),
            replacements: replacements
                .into_iter()
                .map(|item| Replacement {
                    start: item.start as usize,
                    end: item.end as usize,
                    text: String::from_utf16_lossy(&item.text),
                })
                .collect(),
            created,
        }),
        EditsResult::Refused { index, refusal } => Ok(BlockEdits::Refused {
            index: index as f64,
            refusal: json!({"code": refusal.code.as_str(), "message": String::from_utf16_lossy(&refusal.message)}),
        }),
    }
}

pub(super) fn remap(
    map: &ChapterMap,
    source: &str,
    replacements: &[Replacement],
) -> Result<ChapterMap, String> {
    let native = restore(map)?;
    blocks::remap_chapter(
        &PositionsHost,
        &native,
        &source.encode_utf16().collect::<Vec<_>>(),
        &native_replacements(replacements),
    )
    .map(visible)
    .map_err(|error| error.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_preserves_block_identity_and_refuses_unknown_blocks() {
        let source = "<html><body><p>first</p><p>second</p></body></html>";
        let original = open(source).unwrap();
        assert_eq!(original.blocks.len(), 2);
        let edits = [json!({"op":"replace","blockId":original.blocks[0].id,"text":"updated"})];
        let BlockEdits::Applied {
            map, replacements, ..
        } = apply(&original, &edits).unwrap()
        else {
            panic!("edit was refused");
        };
        assert!(map.source.contains("updated"));
        assert_eq!(map.blocks[1].id, original.blocks[1].id);
        assert!(!replacements.is_empty());
        let refused = apply(&map, &[json!({"op":"delete","blockId":"missing"})]).unwrap();
        assert!(matches!(refused, BlockEdits::Refused { .. }));
    }
}
