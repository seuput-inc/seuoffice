//! EPUB's adapter to the shared PDF structure result store.

use jscompat::{json_value_from_serde, json_value_to_serde};
use main_core::epub::store_handlers::{StoreError, StoredTranslation};
use main_core::packages::pdf_structure::files::SystemEnv;
use main_core::packages::pdf_structure::keys::TranslationBlockKey;
use main_core::packages::pdf_structure::store::{
    JobProgress, JobRecordInput, JobState, MigrateInput, ResultStore, ResultStoreOptions,
    StoreError as ResultStoreError, TranslationBlockInput, TranslationBlockStatus,
    TranslationMetaInput,
};
use main_core::packages::pdf_structure::{JobKind, TranslationMode};
use serde_json::Value;

fn error(error: ResultStoreError) -> StoreError {
    match error {
        ResultStoreError::Type(message) => StoreError::Type(message),
        ResultStoreError::Write(error) => StoreError::Write(error),
    }
}

fn with_store<T>(
    dir: &str,
    work: impl FnOnce(&mut ResultStore) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let _guard = crate::host::d2::RESULT_STORE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut store =
        ResultStore::new(dir, ResultStoreOptions::default(), Box::new(SystemEnv)).map_err(error)?;
    work(&mut store)
}

fn mode(mode: Option<TranslationMode>) -> TranslationMode {
    mode.unwrap_or(TranslationMode::Translate)
}

fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

fn js_text(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::String(value)) => value.clone(),
        Some(value) => value.to_string(),
    }
}

fn key(value: Option<&Value>) -> Result<TranslationBlockKey, StoreError> {
    let value = value.ok_or_else(|| {
        StoreError::Type("Cannot read properties of undefined (reading 'occurrence')".into())
    })?;
    if value.is_null() {
        return Err(StoreError::Type(
            "Cannot read properties of null (reading 'occurrence')".into(),
        ));
    }
    let occurrence = value
        .get("occurrence")
        .and_then(Value::as_f64)
        .ok_or_else(|| {
            StoreError::Type(format!(
                "result store: invalid block occurrence {}",
                js_text(value.get("occurrence"))
            ))
        })?;
    if !occurrence.is_finite() || occurrence.fract() != 0.0 || occurrence < 0.0 {
        return Err(StoreError::Type(format!(
            "result store: invalid block occurrence {}",
            jscompat::number_to_string(occurrence)
        )));
    }
    let segment = |field, label| {
        text(value.get(field)).ok_or_else(|| {
            let raw = value
                .get(field)
                .map(Value::to_string)
                .unwrap_or_else(|| "undefined".into());
            StoreError::Type(format!("result store: invalid {label} {raw}"))
        })
    };
    Ok(TranslationBlockKey {
        page_fingerprint: segment("pageFingerprint", "page fingerprint")?,
        text_hash: segment("textHash", "text hash")?,
        occurrence,
    })
}

fn block(value: &Value) -> Result<TranslationBlockInput, StoreError> {
    let status = match value.get("status").and_then(Value::as_str) {
        Some("translated") => TranslationBlockStatus::Translated,
        Some("failed") => TranslationBlockStatus::Failed,
        _ => {
            return Err(StoreError::Type(format!(
                "result store: invalid block status {}",
                js_text(value.get("status"))
            )));
        }
    };
    let edited = value.get("edited").map(|v| v.as_bool().unwrap_or(false));
    let block_text = text(value.get("text"));
    if status == TranslationBlockStatus::Translated && block_text.is_none() {
        return Err(StoreError::Type(
            "result store: a translated block needs its text".into(),
        ));
    }
    if edited.is_some()
        && (edited != Some(true)
            || status != TranslationBlockStatus::Translated
            || value.get("runs").is_some())
    {
        return Err(StoreError::Type(
            "result store: an edited block is translated plain text without runs".into(),
        ));
    }
    if edited.is_some()
        && block_text
            .as_deref()
            .is_none_or(|s| jscompat::trim(s).is_empty())
    {
        return Err(StoreError::Type(
            "result store: an edited block needs its text".into(),
        ));
    }
    Ok(TranslationBlockInput {
        key: key(value.get("key"))?,
        status,
        text: block_text,
        runs: value.get("runs").map(json_value_from_serde),
        reason: text(value.get("reason")),
        edited,
        source_sentence_ends: value.get("sourceSentenceEnds").map(json_value_from_serde),
        sentence_ends: value.get("sentenceEnds").map(json_value_from_serde),
    })
}

pub(super) fn store_list_translations(
    dir: &str,
    doc_hash: &str,
    which: Option<TranslationMode>,
) -> Result<Vec<String>, StoreError> {
    with_store(dir, |store| {
        Ok(store.list_translations(doc_hash, mode(which)))
    })
}

pub(super) fn store_read_translation(
    dir: &str,
    doc_hash: &str,
    lang: &str,
    which: Option<TranslationMode>,
) -> Result<Option<StoredTranslation>, StoreError> {
    with_store(dir, |store| {
        Ok(store
            .read_translation(doc_hash, lang, mode(which))
            .map_err(error)?
            .map(|translation| StoredTranslation {
                lang: translation.lang,
                provider: translation.provider,
                model: translation.model,
                blocks: translation
                    .blocks
                    .values()
                    .map(|block| json_value_to_serde(&block.to_value()))
                    .collect(),
            }))
    })
}

pub(super) fn store_unfinished_jobs(dir: &str, doc_hash: &str) -> Result<Vec<Value>, StoreError> {
    with_store(dir, |store| {
        Ok(store
            .unfinished_jobs(doc_hash)
            .iter()
            .map(|job| json_value_to_serde(&job.to_value()))
            .collect())
    })
}

pub(super) fn store_write_translation_meta(
    dir: &str,
    doc_hash: &str,
    lang: &str,
    provider: &str,
    model: &str,
    which: Option<TranslationMode>,
) -> Result<(), StoreError> {
    with_store(dir, |store| {
        store
            .write_translation_meta(
                doc_hash,
                lang,
                &TranslationMetaInput {
                    provider: provider.into(),
                    model: model.into(),
                },
                mode(which),
            )
            .map_err(error)
    })
}

pub(super) fn store_write_translation_blocks(
    dir: &str,
    doc_hash: &str,
    lang: &str,
    blocks: &[Value],
    which: Option<TranslationMode>,
) -> Result<(), StoreError> {
    with_store(dir, |store| {
        for item in blocks {
            store
                .write_translation_block(doc_hash, lang, &block(item)?, mode(which))
                .map_err(error)?;
        }
        Ok(())
    })
}

pub(super) fn store_delete_translation(
    dir: &str,
    doc_hash: &str,
    lang: &str,
    which: Option<TranslationMode>,
) -> Result<(), StoreError> {
    with_store(dir, |store| {
        store
            .delete_translation(doc_hash, lang, mode(which))
            .map_err(error)
    })
}

pub(super) fn store_delete_translation_except_edited(
    dir: &str,
    doc_hash: &str,
    lang: &str,
    which: Option<TranslationMode>,
) -> Result<f64, StoreError> {
    with_store(dir, |store| {
        store
            .delete_translation_except_edited(doc_hash, lang, mode(which))
            .map_err(error)
    })
}

fn job_input(job: &Value) -> Result<JobRecordInput, StoreError> {
    let kind = job
        .get("kind")
        .and_then(Value::as_str)
        .and_then(JobKind::parse)
        .ok_or_else(|| {
            StoreError::Type(format!(
                "result store: invalid job kind {}",
                js_text(job.get("kind"))
            ))
        })?;
    let which =
        match job.get("mode") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_str().and_then(TranslationMode::parse).ok_or_else(
                || {
                    StoreError::Type(format!(
                        "result store: a {} job has no mode {}",
                        kind.as_str(),
                        value
                    ))
                },
            )?),
        };
    let state = match job.get("state").and_then(Value::as_str) {
        Some("running") => JobState::Running,
        Some("stopped") => JobState::Stopped,
        Some("complete") => JobState::Complete,
        _ => {
            return Err(StoreError::Type(format!(
                "result store: invalid job state {}",
                js_text(job.get("state"))
            )));
        }
    };
    let outputs = match job.get("outputs") {
        None | Some(Value::Null) => None,
        Some(Value::Array(values)) => Some(
            values
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| StoreError::Type("result store: invalid job output".into()))
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        _ => return Err(StoreError::Type("result store: invalid job outputs".into())),
    };
    let progress = match job.get("progress") {
        None => None,
        Some(value) => Some(JobProgress {
            done: value
                .get("done")
                .and_then(Value::as_f64)
                .ok_or_else(|| StoreError::Type("result store: invalid job progress".into()))?,
            total: value
                .get("total")
                .and_then(Value::as_f64)
                .ok_or_else(|| StoreError::Type("result store: invalid job progress".into()))?,
        }),
    };
    Ok(JobRecordInput {
        kind,
        lang: text(job.get("lang")),
        run_id: text(job.get("runId")),
        mode: which,
        state,
        last_error: text(job.get("last_error")),
        resets_at: job.get("resetsAt").and_then(Value::as_f64),
        outputs,
        progress,
    })
}

pub(super) fn store_write_job(dir: &str, doc_hash: &str, job: &Value) -> Result<(), StoreError> {
    with_store(dir, |store| {
        store.write_job(doc_hash, &job_input(job)?).map_err(error)
    })
}

pub(super) fn store_heartbeat_job(
    dir: &str,
    doc_hash: &str,
    kind: &str,
    key: Option<&str>,
    which: Option<TranslationMode>,
) -> Result<bool, StoreError> {
    let kind = JobKind::parse(kind)
        .ok_or_else(|| StoreError::Type(format!("result store: invalid job kind {kind}")))?;
    with_store(dir, |store| {
        store
            .heartbeat_job(doc_hash, kind, key, mode(which))
            .map_err(error)
    })
}

pub(super) fn store_migrate_document(dir: &str, info: Value) -> Result<Value, StoreError> {
    migrate_document(dir, info, None)
}

pub(super) fn store_migrate_document_owned(
    dir: &str,
    info: Value,
    admit: &mut main_core::packages::pdf_structure::files::StoreAdmission<'_>,
) -> Result<Value, StoreError> {
    migrate_document(dir, info, Some(admit))
}

fn migrate_document(
    dir: &str,
    info: Value,
    admit: Option<&mut main_core::packages::pdf_structure::files::StoreAdmission<'_>>,
) -> Result<Value, StoreError> {
    let required = |name| {
        text(info.get(name))
            .ok_or_else(|| StoreError::Type(format!("result store: invalid {name}")))
    };
    let fingerprints =
        info.get("pageFingerprints")
            .and_then(Value::as_array)
            .ok_or_else(|| StoreError::Type("result store: invalid page fingerprints".into()))?
            .iter()
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    StoreError::Type("result store: invalid page fingerprint".into())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
    let keep = match info.get("keepBlockKeys") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|item| key(Some(item)))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        _ => return Err(StoreError::Type("result store: invalid block keys".into())),
    };
    let input = MigrateInput {
        old_doc_hash: required("oldDocHash")?,
        new_doc_hash: required("newDocHash")?,
        page_fingerprints: fingerprints,
        keep_block_keys: keep,
    };
    with_store(dir, |store| {
        let result = match admit {
            Some(admit) => store.migrate_document_guarded(&input, admit),
            None => store.migrate_document(&input),
        };
        result
            .map(|result| json_value_to_serde(&result.to_value()))
            .map_err(error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn translations_and_jobs_persist_and_migrate() {
        let root = std::env::temp_dir().join(format!(
            "epub-result-store-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let dir = root.to_str().unwrap();
        let key = json!({"pageFingerprint":"chapter", "textHash":"text", "occurrence":0});
        let edited = json!({"key":key, "status":"translated", "text":"corrected", "edited":true});
        let generated = json!({"key":{"pageFingerprint":"chapter", "textHash":"other", "occurrence":0}, "status":"translated", "text":"generated"});
        store_write_translation_meta(dir, "old", "ko", "provider", "model", None).unwrap();
        store_write_translation_blocks(dir, "old", "ko", &[edited, generated], None).unwrap();
        assert_eq!(
            store_list_translations(dir, "old", None).unwrap(),
            vec!["ko"]
        );
        assert_eq!(
            store_read_translation(dir, "old", "ko", None)
                .unwrap()
                .unwrap()
                .blocks
                .len(),
            2
        );
        assert_eq!(
            store_delete_translation_except_edited(dir, "old", "ko", None).unwrap(),
            1.0
        );
        let migration = store_migrate_document(
            dir,
            json!({
                "oldDocHash":"old", "newDocHash":"new", "pageFingerprints":["chapter"],
                "keepBlockKeys":[{"pageFingerprint":"chapter", "textHash":"text", "occurrence":0}]
            }),
        )
        .unwrap();
        assert_eq!(migration["translationBlocks"], 1);
        assert_eq!(
            store_read_translation(dir, "new", "ko", None)
                .unwrap()
                .unwrap()
                .blocks[0]["text"],
            "corrected"
        );
        store_write_job(
            dir,
            "old",
            &json!({"kind":"translate", "lang":"ko", "state":"running"}),
        )
        .unwrap();
        assert!(store_heartbeat_job(dir, "old", "translate", Some("ko"), None).unwrap());
        assert_eq!(store_unfinished_jobs(dir, "old").unwrap().len(), 1);
        store_delete_translation(dir, "old", "ko", None).unwrap();
        assert!(
            store_read_translation(dir, "old", "ko", None)
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failed_block_does_not_erase_a_preceding_write() {
        let root = std::env::temp_dir().join(format!(
            "epub-result-store-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let dir = root.to_str().unwrap();
        let result = store_write_translation_blocks(
            dir,
            "doc",
            "en",
            &[
                json!({"key":{"pageFingerprint":"c", "textHash":"a", "occurrence":0}, "status":"translated", "text":"saved"}),
                json!({"key":{"pageFingerprint":"c", "textHash":"b", "occurrence":0}, "status":"translated"}),
            ],
            None,
        );
        assert_eq!(
            result,
            Err(StoreError::Type(
                "result store: a translated block needs its text".into()
            ))
        );
        assert_eq!(
            store_read_translation(dir, "doc", "en", None)
                .unwrap()
                .unwrap()
                .blocks[0]["text"],
            "saved"
        );
        assert!(store_write_translation_meta(dir, "../outside", "en", "p", "m", None).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancelled_migration_preserves_source_and_publishes_no_target_entry() {
        let root =
            std::env::temp_dir().join(format!("epub-owned-migration-{}", jscompat::random_uuid()));
        let dir = root.to_str().unwrap();
        store_write_translation_meta(dir, "old", "ko", "provider", "model", None).unwrap();
        store_write_translation_blocks(
            dir,
            "old",
            "ko",
            &[json!({
                "key":{"pageFingerprint":"chapter","textHash":"text","occurrence":0},
                "status":"translated","text":"retained","edited":true
            })],
            None,
        )
        .unwrap();
        let error = store_migrate_document_owned(
            dir,
            json!({
                "oldDocHash":"old","newDocHash":"new","pageFingerprints":["chapter"]
            }),
            &mut |_| {
                Err(main_core::node_fs::NodeError {
                    code: None,
                    message: "owner released".into(),
                })
            },
        )
        .unwrap_err();
        assert!(matches!(error, StoreError::Write(_)));
        assert!(
            store_read_translation(dir, "new", "ko", None)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store_read_translation(dir, "old", "ko", None)
                .unwrap()
                .unwrap()
                .blocks[0]["text"],
            "retained"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admitted_migration_keeps_translation_payloads_and_provider_fields() {
        let root =
            std::env::temp_dir().join(format!("epub-owned-migration-{}", jscompat::random_uuid()));
        let dir = root.to_str().unwrap();
        store_write_translation_meta(dir, "old", "ko", "provider", "model", None).unwrap();
        store_write_translation_blocks(
            dir,
            "old",
            "ko",
            &[json!({
                "key":{"pageFingerprint":"chapter","textHash":"text","occurrence":0},
                "status":"translated","text":"retained","edited":true
            })],
            None,
        )
        .unwrap();
        let mut effects = 0;
        let result = store_migrate_document_owned(
            dir,
            json!({
                "oldDocHash":"old","newDocHash":"new","pageFingerprints":["chapter"]
            }),
            &mut |effect| {
                effects += 1;
                effect()
            },
        )
        .unwrap();
        assert!(effects > 0);
        assert_eq!(result["translationBlocks"], 1);
        let source = store_read_translation(dir, "old", "ko", None)
            .unwrap()
            .unwrap();
        let migrated = store_read_translation(dir, "new", "ko", None)
            .unwrap()
            .unwrap();
        assert_eq!(migrated, source);
        std::fs::remove_dir_all(root).unwrap();
    }
}
