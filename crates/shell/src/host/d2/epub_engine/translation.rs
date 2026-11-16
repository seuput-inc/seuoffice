use epub_engine::open::{EPUB_ZIP_LIMITS, EpubOpenResult, open_epub};
use epub_engine::pdf_structure::TranslationMode;
use epub_engine::translate::book_units::epub_translation_units;
use epub_engine::translate::keys::epub_block_key_string;
use epub_engine::translate::migrate::epub_migration_info;
use epub_engine::translate::output::{
    BuildTranslatedEpubOptions, BuildTranslatedEpubResult, EpubTranslationOutcome,
    EpubTranslationOutputKind, build_translated_epub,
};
use epub_engine::translate::units::EpubUnitSource;
use serde_json::{Value, json};
use shell_ipc_d2::epub::hosts::{Bytes, Translated, TranslatedRequest};
use std::collections::HashMap;

fn opened(bytes: &[u8]) -> Result<epub_engine::types::EpubBook, String> {
    match open_epub(bytes, &EPUB_ZIP_LIMITS) {
        EpubOpenResult::Opened(book) => Ok(book),
        EpubOpenResult::Refused { reason, .. } => Err(reason.as_str().to_owned()),
    }
}

fn key(key: &epub_engine::pdf_structure::TranslationBlockKey) -> Value {
    json!({"pageFingerprint": key.page_fingerprint, "textHash": key.text_hash, "occurrence": key.occurrence})
}

pub(super) fn units(bytes: Bytes, chapters: Option<&[usize]>) -> Result<Value, String> {
    let book = opened(bytes.as_slice())?;
    let chapters = chapters.map(|indices| {
        indices
            .iter()
            .map(|index| *index as f64)
            .collect::<Vec<_>>()
    });
    let found =
        epub_translation_units(&book, chapters.as_deref()).map_err(|error| error.message)?;
    let units: Vec<_> = found.units.iter().map(|unit| {
        let text = String::from_utf16_lossy(&unit.text);
        let marks = |items: &[epub_engine::notation::MarkSource]| items.iter().map(|mark| json!({
            "startTag": String::from_utf16_lossy(&mark.start_tag), "endTag": String::from_utf16_lossy(&mark.end_tag)
        })).collect::<Vec<_>>();
        let placeholders: Vec<_> = unit.context.placeholders.iter().map(|p| json!({
            "n": p.n, "tag": p.tag, "empty": p.empty, "startTag": String::from_utf16_lossy(&p.start_tag),
            "endTag": String::from_utf16_lossy(&p.end_tag), "outer": String::from_utf16_lossy(&p.outer)
        })).collect();
        let sources: Vec<_> = unit.sources.iter().map(|source| match source {
            EpubUnitSource::Block { spine_index, path, block_id } => json!({"kind":"block","spineIndex":spine_index,"path":path,"blockId":block_id}),
            EpubUnitSource::Title { index } => json!({"kind":"title","index":index}),
            EpubUnitSource::Nav { index } => json!({"kind":"nav","index":index}),
            EpubUnitSource::Ncx { index } => json!({"kind":"ncx","index":index}),
        }).collect();
        json!({"runs":[{"kind":"text","text":text}],"pageFingerprint":unit.page_fingerprint,
            "page":unit.page,"key":key(&unit.key),"text":text,"plainText":String::from_utf16_lossy(&unit.plain_text),
            "context":{"placeholders":placeholders,"bold":marks(&unit.context.bold),"italic":marks(&unit.context.italic),
                "breaks":unit.context.breaks.iter().map(|b| String::from_utf16_lossy(b)).collect::<Vec<_>>()},"sources":sources})
    }).collect();
    let skipped: Vec<_> = found
        .skipped
        .iter()
        .map(|s| json!({"index":s.index,"path":s.path,"reason":s.reason}))
        .collect();
    Ok(json!({"units":units,"skipped":skipped}))
}

pub(super) fn migration(bytes: Bytes, old: &str, new: &str) -> Result<Value, String> {
    let book = opened(bytes.as_slice())?;
    let units = epub_translation_units(&book, None).map_err(|error| error.message)?;
    let paths: Vec<_> = book
        .info
        .spine
        .items
        .iter()
        .map(|item| item.path.clone())
        .collect();
    let info = epub_migration_info(
        &paths,
        units.units.into_iter().map(|unit| unit.key).collect(),
        old,
        new,
    );
    let keys: Vec<_> = info.block_keys.iter().map(key).collect();
    Ok(
        json!({"oldDocHash":info.input.old_doc_hash,"newDocHash":info.input.new_doc_hash,
        "pageFingerprints":info.input.page_fingerprints,"blockKeys":keys,"keepBlockKeys":keys}),
    )
}

pub(super) fn build(bytes: Bytes, request: TranslatedRequest) -> Result<Translated, String> {
    let mut results = HashMap::new();
    for row in &request.results {
        let Some(key) = row.get("key") else {
            return Err("translation result has no key".into());
        };
        let page_fingerprint = key
            .get("pageFingerprint")
            .and_then(Value::as_str)
            .ok_or("invalid translation key")?;
        let text_hash = key
            .get("textHash")
            .and_then(Value::as_str)
            .ok_or("invalid translation key")?;
        let occurrence = key
            .get("occurrence")
            .and_then(Value::as_f64)
            .filter(|n| {
                n.is_finite()
                    && *n >= 0.0
                    && n.fract() == 0.0
                    && *n <= 9_007_199_254_740_991.0
                    && *n <= usize::MAX as f64
            })
            .map(|n| n as usize)
            .ok_or("invalid translation key")?;
        let key = epub_engine::pdf_structure::TranslationBlockKey {
            page_fingerprint: page_fingerprint.into(),
            text_hash: text_hash.into(),
            occurrence,
        };
        results.insert(
            epub_block_key_string(&key),
            EpubTranslationOutcome {
                translated: row.get("status").and_then(Value::as_str) == Some("translated"),
                text: row
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| text.encode_utf16().collect()),
                reason: row.get("reason").and_then(Value::as_str).map(str::to_owned),
            },
        );
    }
    let chapters = request
        .chapters
        .as_ref()
        .map(|items| items.iter().map(|n| *n as f64).collect::<Vec<_>>());
    let kind = match request.kind.as_str() {
        "translated" => EpubTranslationOutputKind::Translated,
        "bilingual" => EpubTranslationOutputKind::Bilingual,
        _ => return Err("invalid translation output kind".into()),
    };
    let uuid = jscompat::random_uuid();
    let now = main_core::packages::pdf_structure::files::system_now_ms();
    let options = BuildTranslatedEpubOptions {
        limits: &EPUB_ZIP_LIMITS,
        kind,
        lang: &request.lang,
        chapters: chapters.as_deref(),
        results: &results,
        now,
        uuid: &uuid,
        mode: if request.easy {
            TranslationMode::Easy
        } else {
            TranslationMode::Translate
        },
    };
    match build_translated_epub(bytes.as_slice(), &options).map_err(|error| error.message)? {
        BuildTranslatedEpubResult::Refused { reason, detail } => Ok(Translated::Refused {
            reason: reason.as_str().into(),
            detail,
        }),
        BuildTranslatedEpubResult::Built {
            bytes,
            report,
            identifier,
        } => Ok(Translated::Built {
            bytes,
            identifier,
            report: json!({
                "translatedBlocks":report.translated_blocks,
                "untranslatedBlocks":report.untranslated_blocks.iter().map(|b| json!({"chapter":b.chapter,"path":b.path,"blockId":b.block_id,"reason":b.reason})).collect::<Vec<_>>(),
                "originalChapters":report.original_chapters.iter().map(|c| json!({"chapter":c.chapter,"path":c.path,"reason":c.reason})).collect::<Vec<_>>()
            }),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn invalid_output_kind_does_not_create_a_book() {
        let request = TranslatedRequest {
            kind: "unknown".into(),
            lang: "en".into(),
            chapters: None,
            results: Vec::new(),
            easy: false,
        };
        assert!(build(Arc::new(Vec::new()), request).is_err());
    }

    #[test]
    fn integral_float_occurrence_reaches_book_validation() {
        let request = TranslatedRequest {
            kind: "translated".into(),
            lang: "en".into(),
            chapters: None,
            easy: false,
            results: vec![
                json!({"key":{"pageFingerprint":"p","textHash":"h","occurrence":0.0},"status":"translated","text":"ok"}),
            ],
        };
        assert!(matches!(
            build(Arc::new(Vec::new()), request),
            Ok(Translated::Refused { .. })
        ));
    }
}
