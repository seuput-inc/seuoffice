//! Production PDF structure result store adapter.

use jscompat::{JsObject, JsonValue, json_value_from_serde, json_value_to_serde};
use main_core::packages::pdf_structure::files::SystemEnv;
use main_core::packages::pdf_structure::keys::TranslationBlockKey;
use main_core::packages::pdf_structure::reading::{
    HighlightBlockInput, HighlightBlockStatus, HighlightLabel, HighlightRun, HighlightSuggestion,
    HighlightSuggestionState, PassageCardInput, PassageKind,
};
use main_core::packages::pdf_structure::store::{
    AiReadMeta, JobProgress, JobRecordInput, JobState, MigrateInput, ResultStore,
    ResultStoreOptions, StoreError as CoreError, TranslationBlockInput, TranslationBlockStatus,
    TranslationMetaInput,
};
use main_core::packages::pdf_structure::{JobKind, TranslationMode};
use serde_json::Value;
use shell_contract::IpcValue;
use shell_ipc_d2::pdf::tools::{StoreError, StoreMethod};
use std::path::PathBuf;

type R<T> = Result<T, StoreError>;
fn bad(what: &str) -> StoreError {
    StoreError::Type(format!("argument is not {what}"))
}
fn fail(error: CoreError) -> StoreError {
    match error {
        CoreError::Type(message) => StoreError::Type(message),
        CoreError::Write(error) => StoreError::Write(error.message),
    }
}
fn text(value: Option<&Value>) -> R<&str> {
    value.and_then(Value::as_str).ok_or_else(|| bad("a string"))
}
fn number(value: Option<&Value>) -> R<f64> {
    value.and_then(Value::as_f64).ok_or_else(|| bad("a number"))
}
fn string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}
fn array(value: Option<&Value>) -> R<&Vec<Value>> {
    value
        .and_then(Value::as_array)
        .ok_or_else(|| bad("an array"))
}
fn strings(value: Option<&Value>) -> R<Vec<String>> {
    array(value)?
        .iter()
        .map(|v| text(Some(v)).map(str::to_owned))
        .collect()
}
fn numbers(value: Option<&Value>) -> R<Vec<f64>> {
    array(value)?.iter().map(|v| number(Some(v))).collect()
}
fn mode(args: &[Value], i: usize) -> R<TranslationMode> {
    match args.get(i).filter(|v| !v.is_null()) {
        None => Ok(TranslationMode::Translate),
        Some(value) => TranslationMode::parse(text(Some(value))?).ok_or_else(|| bad("a mode")),
    }
}
fn kind(value: Option<&Value>) -> R<JobKind> {
    JobKind::parse(text(value)?).ok_or_else(|| bad("a job kind"))
}
fn key(value: Option<&Value>) -> R<TranslationBlockKey> {
    let value = value.ok_or_else(|| bad("a block key"))?;
    Ok(TranslationBlockKey {
        page_fingerprint: text(value.get("pageFingerprint"))?.to_owned(),
        text_hash: text(value.get("textHash"))?.to_owned(),
        occurrence: number(value.get("occurrence"))?,
    })
}
fn block(value: &Value) -> R<TranslationBlockInput> {
    let status = match text(value.get("status"))? {
        "translated" => TranslationBlockStatus::Translated,
        "failed" => TranslationBlockStatus::Failed,
        _ => return Err(bad("a block status")),
    };
    Ok(TranslationBlockInput {
        key: key(value.get("key"))?,
        status,
        text: string(value.get("text")),
        runs: value.get("runs").map(json_value_from_serde),
        reason: string(value.get("reason")),
        edited: value.get("edited").and_then(Value::as_bool),
        source_sentence_ends: value.get("sourceSentenceEnds").map(json_value_from_serde),
        sentence_ends: value.get("sentenceEnds").map(json_value_from_serde),
    })
}
fn job(value: &Value) -> R<JobRecordInput> {
    let state = match text(value.get("state"))? {
        "idle" => JobState::Idle,
        "running" => JobState::Running,
        "stopped" => JobState::Stopped,
        "complete" => JobState::Complete,
        _ => return Err(bad("a job state")),
    };
    let mode = value
        .get("mode")
        .map(|v| TranslationMode::parse(text(Some(v))?).ok_or_else(|| bad("a mode")))
        .transpose()?;
    let progress = value
        .get("progress")
        .map(|v| -> R<JobProgress> {
            Ok(JobProgress {
                done: number(v.get("done"))?,
                total: number(v.get("total"))?,
            })
        })
        .transpose()?;
    Ok(JobRecordInput {
        kind: kind(value.get("kind"))?,
        lang: string(value.get("lang")),
        run_id: string(value.get("runId")),
        mode,
        state,
        last_error: string(value.get("last_error")),
        resets_at: value.get("resetsAt").and_then(Value::as_f64),
        outputs: value.get("outputs").map(|v| strings(Some(v))).transpose()?,
        progress,
    })
}
fn passage(value: &Value) -> R<PassageCardInput> {
    let kind = match text(value.get("kind"))? {
        "explain" => PassageKind::Explain,
        "translate" => PassageKind::Translate,
        _ => return Err(bad("a passage kind")),
    };
    let quads = array(value.get("quads"))?
        .iter()
        .map(|q| numbers(Some(q)))
        .collect::<R<Vec<_>>>()?;
    Ok(PassageCardInput {
        id: text(value.get("id"))?.to_owned(),
        kind,
        page: number(value.get("page"))?,
        page_fingerprint: text(value.get("pageFingerprint"))?.to_owned(),
        quads,
        excerpt: text(value.get("excerpt"))?.to_owned(),
        passage_hash: text(value.get("passageHash"))?.to_owned(),
        lang: string(value.get("lang")),
        text: text(value.get("text"))?.to_owned(),
        provider: text(value.get("provider"))?.to_owned(),
        model: text(value.get("model"))?.to_owned(),
        created_at: number(value.get("createdAt"))?,
        saved_as_note: value.get("savedAsNote").and_then(Value::as_bool),
    })
}
fn run(value: &Value) -> R<HighlightRun> {
    let labels = array(value.get("labels"))?
        .iter()
        .map(|label| {
            Ok(HighlightLabel {
                id: text(label.get("id"))?.to_owned(),
                name: text(label.get("name"))?.to_owned(),
                color: numbers(label.get("color"))?
                    .try_into()
                    .map_err(|_| bad("a color"))?,
            })
        })
        .collect::<R<Vec<_>>>()?;
    Ok(HighlightRun {
        run_id: text(value.get("runId"))?.to_owned(),
        preset_id: text(value.get("presetId"))?.to_owned(),
        labels,
        pages: numbers(value.get("pages"))?,
        provider: text(value.get("provider"))?.to_owned(),
        model: text(value.get("model"))?.to_owned(),
        created_at: number(value.get("createdAt"))?,
    })
}
fn suggestion_state(value: Option<&Value>) -> R<HighlightSuggestionState> {
    HighlightSuggestionState::parse(text(value)?).ok_or_else(|| bad("a suggestion state"))
}
fn highlight_block(value: &Value) -> R<HighlightBlockInput> {
    let status = match text(value.get("status"))? {
        "done" => HighlightBlockStatus::Done,
        "failed" => HighlightBlockStatus::Failed,
        _ => return Err(bad("a highlight status")),
    };
    let suggestions = array(value.get("suggestions"))?
        .iter()
        .map(|s| {
            Ok(HighlightSuggestion {
                id: text(s.get("id"))?.to_owned(),
                label_id: text(s.get("labelId"))?.to_owned(),
                quote: text(s.get("quote"))?.to_owned(),
                state: suggestion_state(s.get("state"))?,
            })
        })
        .collect::<R<Vec<_>>>()?;
    Ok(HighlightBlockInput {
        key: key(value.get("key"))?,
        status,
        suggestions,
    })
}
fn to_json(value: &IpcValue) -> R<Value> {
    Ok(match value {
        IpcValue::Undefined | IpcValue::Null => Value::Null,
        IpcValue::Bool(v) => Value::Bool(*v),
        IpcValue::Number(v) => serde_json::Number::from_f64(*v).map_or(Value::Null, Value::Number),
        IpcValue::Date(v) => jscompat::to_iso_string(*v).map_or(Value::Null, Value::String),
        IpcValue::String(v) => Value::String(v.clone()),
        IpcValue::Array(v) => Value::Array(v.iter().map(to_json).collect::<R<_>>()?),
        IpcValue::Object(v) => Value::Object(
            v.iter()
                .filter(|(_, v)| !matches!(v, IpcValue::Undefined))
                .map(|(k, v)| Ok((k.clone(), to_json(v)?)))
                .collect::<R<_>>()?,
        ),
        IpcValue::Bytes(shell_contract::BytesKind::ArrayBuffer, _) => {
            Value::Object(Default::default())
        }
        IpcValue::Bytes(shell_contract::BytesKind::Uint8Array, bytes) => Value::Object(
            bytes
                .iter()
                .enumerate()
                .map(|(i, byte)| (i.to_string(), Value::from(*byte)))
                .collect(),
        ),
    })
}
fn from_json(value: Value) -> IpcValue {
    match value {
        Value::Null => IpcValue::Null,
        Value::Bool(v) => IpcValue::Bool(v),
        Value::Number(v) => IpcValue::Number(v.as_f64().unwrap_or(f64::NAN)),
        Value::String(v) => IpcValue::String(v),
        Value::Array(v) => IpcValue::Array(v.into_iter().map(from_json).collect()),
        Value::Object(v) => {
            IpcValue::Object(v.into_iter().map(|(k, v)| (k, from_json(v))).collect())
        }
    }
}
fn native(value: JsonValue) -> Value {
    json_value_to_serde(&value)
}
fn values<V>(map: &jscompat::JsMap<V>, f: impl Fn(&V) -> JsonValue) -> Value {
    Value::Array(map.iter().map(|(_, v)| native(f(v))).collect())
}

pub(super) fn store(user_data: PathBuf, method: StoreMethod, args: Vec<IpcValue>) -> R<IpcValue> {
    let args = args.iter().map(to_json).collect::<R<Vec<_>>>()?;
    let dir = user_data
        .to_str()
        .ok_or_else(|| bad("a UTF-8 user-data path"))?;
    let _guard = crate::host::d2::RESULT_STORE_LOCK
        .lock()
        .map_err(|e| StoreError::Other(e.to_string()))?;
    let mut store =
        ResultStore::new(dir, ResultStoreOptions::default(), Box::new(SystemEnv)).map_err(fail)?;
    let arg = |i| args.get(i);
    let doc = || text(arg(0));
    use StoreMethod::*;
    let result = match method {
        ListTranslations => Value::Array(
            store
                .list_translations(doc()?, mode(&args, 1)?)
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
        ReadTranslation => match store
            .read_translation(doc()?, text(arg(1))?, mode(&args, 2)?)
            .map_err(fail)?
        {
            None => Value::Null,
            Some(t) => {
                serde_json::json!({"lang": t.lang, "provider": t.provider, "model": t.model, "blocks": values(&t.blocks, |b| b.to_value())})
            }
        },
        UnfinishedJobs => Value::Array(
            store
                .unfinished_jobs(doc()?)
                .iter()
                .map(|v| native(v.to_value()))
                .collect(),
        ),
        ReadAiReadPages => values(&store.read_ai_read_pages(doc()?), |v| v.to_value()),
        WriteAiReadPage => {
            let meta = arg(3);
            let meta = AiReadMeta {
                provider: meta.and_then(|m| string(m.get("provider"))),
                model: meta.and_then(|m| string(m.get("model"))),
            };
            store
                .write_ai_read_page(
                    doc()?,
                    text(arg(1))?,
                    &json_value_from_serde(arg(2).unwrap_or(&Value::Null)),
                    &meta,
                )
                .map_err(fail)?;
            Value::Null
        }
        WriteTranslationMeta => {
            let v = arg(2).ok_or_else(|| bad("meta"))?;
            let meta = TranslationMetaInput {
                provider: text(v.get("provider"))?.to_owned(),
                model: text(v.get("model"))?.to_owned(),
            };
            store
                .write_translation_meta(doc()?, text(arg(1))?, &meta, mode(&args, 3)?)
                .map_err(fail)?;
            Value::Null
        }
        WriteTranslationBlocks => {
            let blocks = array(arg(2))?.iter().map(block).collect::<R<Vec<_>>>()?;
            store
                .write_translation_blocks(doc()?, text(arg(1))?, &blocks, mode(&args, 3)?)
                .map_err(fail)?;
            Value::Null
        }
        DeleteTranslation => {
            store
                .delete_translation(doc()?, text(arg(1))?, mode(&args, 2)?)
                .map_err(fail)?;
            Value::Null
        }
        WriteJob => {
            store
                .write_job(doc()?, &job(arg(1).ok_or_else(|| bad("a job"))?)?)
                .map_err(fail)?;
            Value::Null
        }
        HeartbeatJob => Value::Bool(
            store
                .heartbeat_job(
                    doc()?,
                    kind(arg(1))?,
                    arg(2).and_then(Value::as_str),
                    mode(&args, 3)?,
                )
                .map_err(fail)?,
        ),
        WritePassage => {
            store
                .write_passage(doc()?, &passage(arg(1).ok_or_else(|| bad("a card"))?)?)
                .map_err(fail)?;
            Value::Null
        }
        DeletePassage => {
            store.delete_passage(doc()?, text(arg(1))?).map_err(fail)?;
            Value::Null
        }
        WriteHighlightRun => {
            store
                .write_highlight_run(doc()?, &run(arg(1).ok_or_else(|| bad("a run"))?)?)
                .map_err(fail)?;
            Value::Null
        }
        WriteHighlightBlocks => {
            let blocks = array(arg(2))?
                .iter()
                .map(highlight_block)
                .collect::<R<Vec<_>>>()?;
            store
                .write_highlight_blocks(doc()?, text(arg(1))?, &blocks)
                .map_err(fail)?;
            Value::Null
        }
        SetHighlightSuggestionStates => {
            let mut states = JsObject::new();
            if let Some(Value::Object(map)) = arg(3) {
                for (id, state) in map {
                    states.set(id.as_str(), suggestion_state(Some(state))?);
                }
            }
            Value::Bool(
                store
                    .set_highlight_suggestion_states(doc()?, text(arg(1))?, &key(arg(2))?, &states)
                    .map_err(fail)?,
            )
        }
        DeleteHighlightRun => {
            store
                .delete_highlight_run(doc()?, text(arg(1))?)
                .map_err(fail)?;
            Value::Null
        }
        WriteInsights => {
            let entry = json_value_from_serde(arg(1).ok_or_else(|| bad("an entry"))?);
            store.write_insights(doc()?, &entry).map_err(fail)?;
            Value::Null
        }
        WriteEditedTranslationBlock => {
            store
                .write_edited_translation_block(
                    doc()?,
                    text(arg(1))?,
                    &key(arg(2))?,
                    text(arg(3))?,
                    mode(&args, 4)?,
                )
                .map_err(fail)?;
            Value::Null
        }
        DeleteTranslationExceptEdited => Value::from(
            store
                .delete_translation_except_edited(doc()?, text(arg(1))?, mode(&args, 2)?)
                .map_err(fail)?,
        ),
        ReadPassages => Value::Array(
            store
                .read_passages(doc()?)
                .iter()
                .map(|v| native(v.to_value()))
                .collect(),
        ),
        ListHighlightRuns => Value::Array(
            store
                .list_highlight_runs(doc()?)
                .iter()
                .map(|v| native(v.to_value()))
                .collect(),
        ),
        ReadHighlightBlocks => values(&store.read_highlight_blocks(doc()?, text(arg(1))?), |v| {
            v.to_value()
        }),
        ReadInsights => store
            .read_insights(doc()?, text(arg(1))?)
            .map_or(Value::Null, |v| native(v.to_value())),
        CountEditedTranslationBlocks => Value::from(
            store
                .count_edited_translation_blocks(
                    doc()?,
                    arg(1).and_then(Value::as_str),
                    mode(&args, 2)?,
                )
                .map_err(fail)?,
        ),
        CountPassages => Value::from(store.count_passages(doc()?)),
        MigrateDocument => {
            let input = arg(0).ok_or_else(|| bad("input"))?;
            let keep = input
                .get("keepBlockKeys")
                .map(|v| {
                    array(Some(v))?
                        .iter()
                        .map(|v| key(Some(v)))
                        .collect::<R<Vec<_>>>()
                })
                .transpose()?;
            let input = MigrateInput {
                old_doc_hash: text(input.get("oldDocHash"))?.to_owned(),
                new_doc_hash: text(input.get("newDocHash"))?.to_owned(),
                page_fingerprints: strings(input.get("pageFingerprints"))?,
                keep_block_keys: keep,
            };
            native(store.migrate_document(&input).map_err(fail)?.to_value())
        }
        DeleteDocument => {
            store.delete_document(doc()?).map_err(fail)?;
            Value::Null
        }
        ListJobs => Value::Array(
            store
                .list_jobs(doc()?)
                .iter()
                .map(|v| native(v.to_value()))
                .collect(),
        ),
        ReadJob => native(
            store
                .read_job(
                    doc()?,
                    kind(arg(1))?,
                    arg(2).and_then(Value::as_str),
                    mode(&args, 3)?,
                )
                .to_value(),
        ),
    };
    Ok(from_json(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn writes_and_reads_real_page_records() {
        let dir = std::env::temp_dir().join(format!(
            "pdf-store-host-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let args = vec![
            IpcValue::String("document".into()),
            IpcValue::String("page".into()),
            IpcValue::Object(vec![("version".into(), IpcValue::Number(1.0))]),
        ];
        store(dir.clone(), StoreMethod::WriteAiReadPage, args).unwrap();
        let pages = store(
            dir.clone(),
            StoreMethod::ReadAiReadPages,
            vec![IpcValue::String("document".into())],
        )
        .unwrap();
        let IpcValue::Array(pages) = pages else {
            panic!("expected page array")
        };
        assert_eq!(pages.len(), 1);
        assert!(
            matches!(&pages[0], IpcValue::Object(fields) if fields.iter().any(|(key, value)| key == "pageFingerprint" && *value == IpcValue::String("page".into())))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_unsafe_store_segments() {
        let dir = std::env::temp_dir().join("pdf-store-invalid-key");
        let result = store(
            dir,
            StoreMethod::WriteAiReadPage,
            vec![
                IpcValue::String("../outside".into()),
                IpcValue::String("page".into()),
                IpcValue::Object(vec![("version".into(), IpcValue::Number(1.0))]),
            ],
        );
        assert!(matches!(result, Err(StoreError::Type(_))));
    }
}
