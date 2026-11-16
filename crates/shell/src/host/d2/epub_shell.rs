//! Shell hooks used by the EPUB editor.

use std::sync::Arc;

use main_core::packages::electron_utils::unsaved_files::OpenFileState;
use main_core::packages::i18n::Lang;
use main_core::packages::pdf_structure::files::{SystemEnv, system_now_ms};
use main_core::packages::pdf_structure::store::{ResultStore, ResultStoreOptions};
use shell_contract::ViewId;
use shell_contract::runtime::Thread;
use shell_ipc_d2::epub::hosts::{EpubShell, EpubShellHost};
use shell_ipc_w::editor_files::UiLanguage;
use shell_ipc_w::tabs::TabsState;

use super::{epub_open_files, job_migration};
use crate::host::{w::on_core, w_files::ShellLanguage};

pub(super) fn host(ui: Arc<dyn Thread>) -> EpubShellHost {
    Arc::new(Shell {
        language: ShellLanguage(ui),
    })
}

struct Shell {
    language: ShellLanguage,
}

impl EpubShell for Shell {
    fn ui_lang(&self) -> Lang {
        self.language.lang()
    }

    fn open_elsewhere(&self, path: &str) -> bool {
        let path = path.to_owned();
        on_core(move |provided| {
            provided
                .all::<TabsState>()
                .into_iter()
                .next()
                .is_some_and(|tabs| tabs.is_path_open_elsewhere(&path))
        })
        .unwrap_or(false)
    }

    fn is_path_open_in_tab(&self, path: &str) -> Result<bool, String> {
        let path = path.to_owned();
        on_core(move |provided| {
            let tabs = provided
                .all::<TabsState>()
                .into_iter()
                .next()
                .ok_or_else(|| "The tab manager is not available".to_owned())?;
            Ok(tabs.view_of_file(&path).is_some())
        })
        .ok_or_else(|| "Core services are not available".to_owned())?
    }

    fn migrate_job_records(&self, dir: &str, old: &str, new: &str) -> Result<f64, String> {
        let mut store = ResultStore::new(dir, ResultStoreOptions::default(), Box::new(SystemEnv))
            .map_err(|error| error.message().to_owned())?;
        job_migration::migrate_job_records(&mut store, old, new)
            .map_err(|error| error.message().to_owned())
    }

    fn migrate_job_records_owned(
        &self,
        dir: &str,
        old: &str,
        new: &str,
        admit: &mut main_core::packages::pdf_structure::files::StoreAdmission<'_>,
    ) -> Result<f64, String> {
        let mut store = ResultStore::new(dir, ResultStoreOptions::default(), Box::new(SystemEnv))
            .map_err(|error| error.message().to_owned())?;
        job_migration::migrate_job_records_owned(&mut store, old, new, admit)
            .map_err(|error| error.message().to_owned())
    }

    fn file_saved(&self, view: ViewId, path: &str) {
        let path = path.to_owned();
        on_core(move |provided| {
            if let Some(tabs) = provided.all::<TabsState>().into_iter().next() {
                tabs.file_saved(view, &path);
            }
        });
    }

    fn file_saved_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
        path: &str,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>,
    ) {
        crate::host::d1::shells::file_saved_many_owned_extra(
            origin.clone(),
            destination.clone(),
            participants.to_vec(),
            cancel.clone(),
            pending.clone(),
            additional.to_vec(),
            effects.clone(),
            std::path::PathBuf::from(path),
            false,
            Box::new(move |result| {
                done(result.and_then(|saved| {
                    let path = saved
                        .path
                        .to_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "The saved path is not valid Unicode".to_owned())?;
                    Ok(saved.map_path(|_| path))
                }));
            }),
        );
    }

    fn file_saved_existing_document(
        &self,
        destination: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>,
    ) {
        crate::host::d1::shells::file_saved_existing_document(
            destination,
            participants,
            cancel,
            pending,
            additional,
            effects,
            false,
            Box::new(move |result| {
                done(result.and_then(|saved| {
                    let path = saved
                        .path
                        .to_str()
                        .ok_or("The saved path is not valid Unicode")?
                        .to_owned();
                    Ok(saved.map_path(|_| path))
                }));
            }),
        );
    }

    fn open_files(&self, then: Box<dyn FnOnce(Option<Result<Vec<OpenFileState>, ()>>)>) {
        epub_open_files::open_files(then);
    }

    fn random_token(&self) -> String {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("operating system random source failed");
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn now(&self) -> f64 {
        system_now_ms()
    }
}
