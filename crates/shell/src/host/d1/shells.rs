//! Shell services shared by the D1 editors.

use std::sync::Arc;

use main_core::packages::electron_utils::generate_handoff::{GenerateHandoff, GenerateHandoffTarget};
use main_core::packages::pdf_structure::files::system_now_ms;
use shell_contract::{Hosts, ViewId};
use shell_ipc_d1::mindmap::{MindmapShell, MindmapShellHost};
use shell_ipc_d1::diagram::{DiagramShell, DiagramShellHost};
use shell_ipc_d1::mindmap::PageRenderer;
use shell_ipc_w::projects::Projects;
use shell_ipc_w::tabs::TabsState;

use crate::host::w::on_core;
mod saved;
pub(crate) use saved::{file_saved_owned,file_saved_many_owned,file_saved_many_owned_extra,file_saved_existing_document};

static DELIMITED_IMPORT_LIMIT: std::sync::LazyLock<std::sync::Mutex<main_core::sheets::csv_import_limit::DelimitedImportLimit>> =
    std::sync::LazyLock::new(Default::default);

pub(crate) fn set_delimited_import_limit(bytes: f64) {
    DELIMITED_IMPORT_LIMIT.lock().unwrap_or_else(|error| error.into_inner())
        .set_delimited_import_limit(Some(bytes));
}

pub(crate) fn delimited_import_limit() -> f64 {
    DELIMITED_IMPORT_LIMIT.lock().unwrap_or_else(|error| error.into_inner())
        .delimited_import_limit()
}

pub(super) fn install_mindmap(hosts: &mut Hosts) {
    let shell: MindmapShellHost = Arc::new(Mindmap);
    hosts.insert(shell);
}

pub(super) fn install_diagram(hosts: &mut Hosts, cx: &crate::host::HostContext) {
    let shell: DiagramShellHost = Arc::new(Diagram(cx.clone()));
    hosts.insert(shell);
}

pub(crate) fn new_document_dir(view: ViewId) -> Option<String> {
    on_core(move |provided| {
        let projects = provided.all::<Projects>().into_iter().next()?;
        let placer = projects.new_documents();
        let placement = placer.borrow_mut().placement_for(i64::from(view.0));
        match placement {
            Ok(placement) => placement.map(|placement| placement.dir),
            Err(error) => {
                eprintln!("[shell] could not determine the project folder: {}", error.message);
                None
            }
        }
    }).flatten()
}

pub(crate) fn write_new_document(view: ViewId, name: &str, bytes: &[u8]) -> Result<Option<String>, String> {
    let name = name.to_owned();
    let bytes = bytes.to_vec();
    on_core(move |provided| {
        let tabs = provided.all::<TabsState>().into_iter().next()
            .ok_or_else(|| "The tab manager is not available".to_owned())?;
        if tabs.details_of(view).is_none() {
            return Err("The document view is no longer open".to_owned());
        }
        let projects = provided.all::<Projects>().into_iter().next()
            .ok_or_else(|| "Project services are not available".to_owned())?;
        let placer = projects.new_documents();
        let result = placer.borrow_mut().write_for(i64::from(view.0), &name, &bytes)
            .map_err(|error| error.message());
        result
    }).ok_or_else(|| "Core services are not available".to_owned())?
}

pub(super) fn file_saved(view: ViewId, path: &str, document_links: bool) {
    let path = path.to_owned();
    on_core(move |provided| {
        if let Some(tabs) = provided.all::<TabsState>().into_iter().next()
            && tabs.details_of(view).is_some()
        {
            tabs.file_saved(view, &path);
            if document_links {
                crate::host::menu::note_document_saved(view, &path, false);
            }
            tabs.apply_pending_dir(view, &path);
        }
    });
}

fn title(view: ViewId) -> Option<String> {
    on_core(move |provided| {
        provided.all::<TabsState>().into_iter().next()?.details_of(view).map(|details| details.title)
    }).flatten()
}

pub(super) fn set_title(view: ViewId, title: &str) {
    let title = title.to_owned();
    on_core(move |provided| {
        if let Some(tabs) = provided.all::<TabsState>().into_iter().next() {
            tabs.set_document_title(view, &title);
        }
    });
}

pub(super) fn open_elsewhere(path: &str) -> bool {
    let path = path.to_owned();
    on_core(move |provided| {
        provided.all::<TabsState>().into_iter().next()
            .map(|tabs| tabs.is_path_open_elsewhere(&path))
    }).flatten().unwrap_or(true)
}

pub(super) fn take_handoff(
    consumer: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken,
    target: GenerateHandoffTarget,
) -> Result<Option<GenerateHandoff>, String> {
    let consumer = consumer.clone();
    let cancel = cancel.clone();
    on_core(move |provided| {
        shell_ipc_a::user_skills::take_generate_handoff_owned(provided, target, &consumer, &cancel)
    }).ok_or_else(|| "Core services are not available".to_owned())?
}

pub(super) fn report_handoff(
    consumer: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken,
    target: GenerateHandoffTarget,
    paths: &[String],
) -> Result<bool, String> {
    let consumer = consumer.clone();
    let cancel = cancel.clone();
    let paths = paths.to_vec();
    on_core(move |provided| {
        shell_ipc_a::user_skills::report_generate_run_started_owned(provided, target, &consumer, &cancel, &paths, system_now_ms())
            .map(|event| event.is_some())
    }).ok_or_else(|| "Core services are not available".to_owned())?
}

pub(super) fn reload_view(view: ViewId) {
    on_core(move |provided| {
        if let Some(tabs) = crate::host::shell_tabs::shell_tabs(provided) {
            tabs.reload_view(view);
        }
    });
}

struct WorkbookPasswordPrompt(crate::host::document_password::PasswordPrompt);

impl shell_ipc_d1::sheets::engine::PasswordPrompt for WorkbookPasswordPrompt {
    fn ask(&self, retry: bool) -> Result<Option<String>, shell_ipc_d1::sheets::engine::UnlockFailure> {
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        self.0.ask(retry, Box::new(move |answer| { let _ = send.send(answer); }));
        receive.recv().map_err(|error| shell_ipc_d1::sheets::engine::UnlockFailure::Other(shell_contract::IpcError {
            name: Some("Error".into()), message: error.to_string(),
        }))?.map_err(|message| shell_ipc_d1::sheets::engine::UnlockFailure::Other(shell_contract::IpcError {
            name: Some("Error".into()), message,
        }))
    }

    fn close(&self) {
        self.0.close();
    }
}


struct Mindmap;

impl MindmapShell for Mindmap {
    fn file_saved(&self, view: ViewId, path: &str) { file_saved(view, path, true); }
    fn file_saved_owned(
        &self,origin:&shell_contract::ipc::NativeRequestOwner,destination:&shell_contract::ipc::NativeRequestOwner,
        participants:&[shell_contract::ipc::NativeRequestOwner],cancel:&shell_contract::runtime::CancelToken,pending:&shell_contract::runtime::CancelToken,
        additional:&[shell_contract::runtime::CancelToken],effects:&shell_contract::hosts::OwnedEffectsHost,path:std::path::PathBuf,
        done:Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>,String>)>,
    ) {
        saved::file_saved_many_owned_extra(origin.clone(),destination.clone(),participants.to_vec(),cancel.clone(),pending.clone(),additional.to_vec(),effects.clone(),path,true,done);
    }
    fn file_saved_existing_document(&self, revision: shell_contract::ipc::NativeSaveRevisionAuthority, expected_path: std::path::PathBuf,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>, cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken, additional: Vec<shell_contract::runtime::CancelToken>, effects: shell_contract::hosts::OwnedEffectsHost,
        on_revision: std::rc::Rc<dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt) -> Result<(), String>>,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>) {
        file_saved_existing_document(revision, expected_path, participants, cancel, pending, additional, effects, true, on_revision, done);
    }
    fn make_from_document(&self, cx: shell_contract::ipc::CallContext) {
        let authority = shell_ipc_a::generate::host::GenerateAuthority::renderer(&cx);
        let view = cx.sender.view;
        on_core(move |_| crate::host::menu::make_from_document(view, authority));
    }
    fn recovery_title(&self, view: ViewId) -> Option<String> { title(view) }
    fn provisional_title(&self, view: ViewId, value: &str) { set_title(view, value); }
    fn reload_view(&self, view: ViewId) { reload_view(view); }
    fn new_document_dir(&self, view: ViewId) -> Option<String> { new_document_dir(view) }
    fn has_new_document_writer(&self) -> bool {
        on_core(|provided| !provided.all::<Projects>().is_empty()).unwrap_or(false)
    }
    fn write_new_document_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<String>, String> {
        crate::host::w_projects::write_new_document_owned(origin, destination, cancel, pending, effects, admission, file_name, bytes)
    }
    fn is_path_open_elsewhere(&self, path: &str) -> bool { open_elsewhere(path) }
    fn assert_file_saved(&self, path: String, then: Box<dyn FnOnce(Result<(), String>) + Send>) {
        super::save_guard::check(path, then);
    }
    fn take_generate_handoff_owned(
        &self, consumer: &shell_contract::ipc::NativeRequestOwner, cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Option<GenerateHandoff>, String> {
        take_handoff(consumer, cancel, GenerateHandoffTarget::Mindmap)
    }
    fn report_generate_run_started_owned(
        &self, consumer: &shell_contract::ipc::NativeRequestOwner, cancel: &shell_contract::runtime::CancelToken, paths: &[String],
    ) -> Result<bool, String> {
        report_handoff(consumer, cancel, GenerateHandoffTarget::Mindmap, paths)
    }
}

struct Diagram(crate::host::HostContext);

impl DiagramShell for Diagram {
    fn new_document_dir(&self, view: ViewId) -> Option<String> { new_document_dir(view) }
    fn has_new_document_writer(&self) -> bool {
        on_core(|provided| !provided.all::<Projects>().is_empty()).unwrap_or(false)
    }
    fn write_new_document(&self, view: ViewId, name: &str, bytes: &[u8]) -> Option<Result<Option<String>, String>> {
        Some(write_new_document(view, name, bytes))
    }
    fn write_new_document_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<String>, String> {
        crate::host::w_projects::write_new_document_owned(origin, destination, cancel, pending, effects, admission, file_name, bytes)
    }
    fn file_saved(&self, view: ViewId, path: &str) { file_saved(view, path, false); }
    fn file_saved_owned(
        &self,origin:&shell_contract::ipc::NativeRequestOwner,destination:&shell_contract::ipc::NativeRequestOwner,
        participants:&[shell_contract::ipc::NativeRequestOwner],cancel:&shell_contract::runtime::CancelToken,pending:&shell_contract::runtime::CancelToken,
        additional:&[shell_contract::runtime::CancelToken],effects:&shell_contract::hosts::OwnedEffectsHost,path:&str,
        done:Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>,String>)>,
    ) {
        saved::file_saved_many_owned_extra(origin.clone(),destination.clone(),participants.to_vec(),cancel.clone(),pending.clone(),additional.to_vec(),effects.clone(),path.into(),false,Box::new(move |result| {
            done(result.and_then(|saved| {
                let path = saved.path.to_str().ok_or("saved Diagram path is not Unicode")?.to_owned();
                Ok(saved.map_path(|_| path))
            }));
        }));
    }
    fn file_saved_existing_document(&self, revision: shell_contract::ipc::NativeSaveRevisionAuthority, expected_path: std::path::PathBuf,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>, cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken, additional: Vec<shell_contract::runtime::CancelToken>, effects: shell_contract::hosts::OwnedEffectsHost,
        on_revision: std::rc::Rc<dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt) -> Result<(), String>>,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>) {
        file_saved_existing_document(revision, expected_path, participants, cancel, pending, additional, effects, false, on_revision, Box::new(move |result| {
            done(result.and_then(|saved| {
                let path = saved.path.to_str().ok_or("saved Diagram path is not Unicode")?.to_owned();
                Ok(saved.map_path(|_| path))
            }));
        }));
    }
    fn recovery_title(&self, view: ViewId) -> Option<String> { title(view) }
    fn help_owned(&self, origin: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        _effects: shell_contract::hosts::OwnedEffectsHost, done: Box<dyn FnOnce(Result<(), String>)>) {
        let done = std::rc::Rc::new(std::cell::RefCell::new(Some(done)));
        let callback = done.clone();
        let ran = crate::host::w::on_core_local(move |provided| {
            let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
                if let Some(done) = callback.borrow_mut().take() { done(Err("The tab manager is unavailable".into())); }
                return;
            };
            tabs.open_help_owned(Some("diagram".into()), origin, cancel, pending, Box::new(move |result| {
                if let Some(done) = callback.borrow_mut().take() { done(result.map(|_| ()).map_err(|error| error.0)); }
            }));
        });
        if !ran {
            if let Some(done) = done.borrow_mut().take() { done(Err("The core thread is unavailable".into())); }
        }
    }
    fn open_failed(&self, view: ViewId, message: &str) -> bool {
        let exists = on_core(move |provided| {
            provided.all::<TabsState>().into_iter().next()
                .is_some_and(|tabs| tabs.details_of(view).is_some())
        }).unwrap_or(false);
        if !exists { return false; }
        let message = message.to_owned();
        crate::host::w::post_core(move |provided, services| {
            if let Some(tabs) = provided.all::<TabsState>().into_iter().next() {
                tabs.close_document_without_prompt(view);
            }
            if let Some(dialogs) = services.hosts.get::<shell_contract::DialogsHost>() {
                dialogs.message(None, &shell_contract::hosts::MessageBox {
                    kind: Some("warning".into()),
                    message,
                    buttons: vec!["OK".into()],
                    ..Default::default()
                });
            }
        });
        true
    }
    fn provisional_title(&self, view: ViewId, value: &str) { set_title(view, value); }
    fn reload_view(&self, view: ViewId) { reload_view(view); }
    fn is_path_open_elsewhere(&self, path: &str) -> bool { open_elsewhere(path) }
    fn assert_file_saved(&self, path: String, then: Box<dyn FnOnce(Result<(), String>) + Send>) {
        super::save_guard::check(path, then);
    }
    fn is_headless(&self) -> bool { false }
    fn save_token(&self) -> String {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("operating system random source failed");
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
    fn random_uuid(&self) -> String { jscompat::random_uuid() }
    fn render_pdf(&self, pages: &[main_core::diagram::print_document::PrintPage], title: &str) -> Result<Vec<u8>, String> {
        let html = main_core::diagram::print_document::build_print_html(pages, title);
        super::page_renderer::Renderer(self.0.clone()).page_to_pdf(&html)
    }
}
