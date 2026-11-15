//! Job-record migration from apps/shell/src/main/pdf-structure/job-migration.ts.

use main_core::packages::pdf_structure::{TranslationMode, job_key_of};
use main_core::packages::pdf_structure::store::{JobRecordInput, JobState, JobStatus, ResultStore, StoreError};

pub(super) fn migrate_job_records(
    store: &mut ResultStore,
    old_hash: &str,
    new_hash: &str,
) -> Result<f64, StoreError> {
    migrate_job_records_admitted(store, old_hash, new_hash, None)
}

pub(super) fn migrate_job_records_owned(store: &mut ResultStore, old_hash: &str, new_hash: &str,
    admit: &mut main_core::packages::pdf_structure::files::StoreAdmission<'_>) -> Result<f64, StoreError> {
    migrate_job_records_admitted(store, old_hash, new_hash, Some(admit))
}

fn migrate_job_records_admitted(store: &mut ResultStore, old_hash: &str, new_hash: &str,
    mut admit: Option<&mut main_core::packages::pdf_structure::files::StoreAdmission<'_>>) -> Result<f64, StoreError> {
    let _guard = super::RESULT_STORE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    if old_hash == new_hash {
        return Ok(0.0);
    }
    let mut moved = 0;
    for job in store.list_jobs(old_hash) {
        let key = job_key_of(job.kind.as_str(), job.lang.as_deref(), job.run_id.as_deref());
        let mode = if job.mode_easy { TranslationMode::Easy } else { TranslationMode::Translate };
        let current = store.read_job(new_hash, job.kind, key.as_deref(), mode);
        if current.state != JobState::Idle
            && current.updated_at.unwrap_or(0.0) >= job.updated_at.unwrap_or(0.0)
        {
            continue;
        }
        let mut source = job;
        for _ in 0..3 {
            match admit.as_mut() {
                Some(admit) => store.write_job_guarded(new_hash, &record_of(&source), &mut **admit)?,
                None => store.write_job(new_hash, &record_of(&source))?,
            }
            let again = store.read_job(old_hash, source.kind, key.as_deref(), mode);
            if again.state == JobState::Idle || again.updated_at == source.updated_at {
                break;
            }
            source = again;
        }
        moved += 1;
    }
    Ok(f64::from(moved))
}

fn record_of(job: &JobStatus) -> JobRecordInput {
    JobRecordInput {
        kind: job.kind,
        lang: job.lang.clone(),
        run_id: job.run_id.clone(),
        mode: job.mode_easy.then_some(TranslationMode::Easy),
        state: if job.state == JobState::Idle { JobState::Stopped } else { job.state },
        last_error: job.last_error.clone(),
        resets_at: job.resets_at,
        outputs: Some(job.outputs.clone()),
        progress: job.progress,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use main_core::packages::pdf_structure::{JobKind, TranslationMode};
    use main_core::packages::pdf_structure::files::SystemEnv;
    use main_core::packages::pdf_structure::store::ResultStoreOptions;

    #[test]
    fn migration_keeps_newer_records_and_copies_other_modes() {
        let dir = std::env::temp_dir().join(format!("epub-job-migration-{}", jscompat::random_uuid()));
        let path = dir.to_str().unwrap();
        let mut clock = 1_000.0;
        let mut make_store = || {
            let now = clock;
            clock += 1_000.0;
            ResultStore::new(path, ResultStoreOptions {
                now: Some(Box::new(move || now)),
                ..Default::default()
            }, Box::new(SystemEnv)).unwrap()
        };
        let mut old = make_store();
        let mut new = make_store();
        let record = |mode, text: &str| JobRecordInput {
            kind: JobKind::Translate,
            lang: Some("ko".into()),
            run_id: None,
            mode,
            state: JobState::Complete,
            last_error: None,
            resets_at: None,
            outputs: Some(vec![text.into()]),
            progress: None,
        };
        old.write_job("old", &record(None, "old")).unwrap();
        old.write_job("old", &record(Some(TranslationMode::Easy), "easy")).unwrap();
        new.write_job("new", &record(None, "new")).unwrap();
        assert_eq!(migrate_job_records(&mut new, "old", "new").unwrap(), 1.0);
        assert_eq!(new.read_job("new", JobKind::Translate, Some("ko"), TranslationMode::Translate).outputs, vec!["new"]);
        assert_eq!(new.read_job("new", JobKind::Translate, Some("ko"), TranslationMode::Easy).outputs, vec!["easy"]);
        assert_eq!(migrate_job_records(&mut new, "old", "new").unwrap(), 0.0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod owned_tests {
    use super::*;
    use main_core::packages::pdf_structure::{JobKind, TranslationMode};
    use main_core::packages::pdf_structure::files::SystemEnv;
    use main_core::packages::pdf_structure::store::ResultStoreOptions;

    #[test]
    fn revoked_migration_cannot_publish_a_job_record() {
        let root = std::env::temp_dir().join(format!("epub-owned-jobs-{}", jscompat::random_uuid()));
        let path = root.to_str().unwrap();
        let mut store = ResultStore::new(path, ResultStoreOptions::default(), Box::new(SystemEnv)).unwrap();
        let record = JobRecordInput {
            kind: JobKind::Translate, lang: Some("ko".into()), run_id: None, mode: None,
            state: JobState::Complete, last_error: None, resets_at: None,
            outputs: Some(vec!["retained".into()]), progress: None,
        };
        store.write_job("old", &record).unwrap();
        let result = migrate_job_records_owned(&mut store, "old", "new",
            &mut |_| Err(main_core::node_fs::NodeError { code: None, message: "owner released".into() }));
        assert!(result.is_err());
        assert_eq!(store.read_job("new", JobKind::Translate, Some("ko"), TranslationMode::Translate).state, JobState::Idle);
        assert_eq!(store.read_job("old", JobKind::Translate, Some("ko"), TranslationMode::Translate).outputs, vec!["retained"]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
