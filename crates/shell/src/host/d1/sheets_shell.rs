//! The workbook editor's actual native shell hooks and OS capture provider.
mod capture;
#[cfg(target_os = "linux")]
mod portal;

use std::path::PathBuf;
use std::sync::{Arc, mpsc};

use shell_contract::{Hosts, IpcError, IpcValue, ViewId, WindowId, hosts::OwnedEffectsHost, ipc::NativeRequestOwner, runtime::CancelToken};
use shell_ipc_d1::sheets::engine::{CreateDocumentAnswer, PasswordPrompt, ScreenCaptureHost, SheetsShell, SheetsShellHost, UnlockFailure};
use shell_ipc_w::tabs::TabsState;

use crate::host::{HostContext, document_password, w::{on_core, on_core_services_local, post_core}};
use crate::ipc::native_owner;
use super::shells;

struct Shell {
    cx: HostContext,
    effects: OwnedEffectsHost,
    passwords: Arc<document_password::DocumentPasswordService>,
    fonts: String,
}

pub(super) fn install(hosts: &mut Hosts, cx: &HostContext) -> Result<(), String> {
    let fonts = if cfg!(debug_assertions) {
        let apps = std::env::var_os("SEUOFFICE_APPS_DIR").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../apps")));
        jscompat::node_path::host::resolve(&[&apps.join("shell").to_string_lossy(), "..", "..", "packages/ui/template-fonts"])
    } else {
        crate::paths::resources_path(&cx.app).ok_or("Application resources are unavailable")?
            .join("template-fonts").into_os_string().into_string().map_err(|_| "The template font path is not Unicode")?
    };
    let shell: SheetsShellHost = Arc::new(Shell {
        cx: cx.clone(), effects: native_owner::effects_host(cx),
        passwords: hosts.get::<Arc<document_password::DocumentPasswordService>>().cloned()
            .ok_or("The shared document password service must be installed before Sheets")?, fonts,
    });
    hosts.insert(shell);
    let capture: ScreenCaptureHost = Arc::new(capture::Capture::new(cx));
    hosts.insert(capture);
    Ok(())
}

struct Prompt(document_password::PasswordPrompt);
impl PasswordPrompt for Prompt {
    fn ask(&self, retry: bool) -> Result<Option<String>, UnlockFailure> {
        let (send, receive) = mpsc::sync_channel(1);
        self.0.ask(retry, Box::new(move |result| { let _ = send.send(result); }));
        receive.recv().map_err(|error| UnlockFailure::Other(IpcError { name: Some("Error".into()), message: error.to_string() }))?
            .map_err(|message| UnlockFailure::Other(IpcError { name: Some("Error".into()), message }))
    }
    fn close(&self) { self.0.close(); }
}

fn prepare_publication(
    output: shell_contract::ipc::SavedPublication, written: Option<shell_contract::ipc::NativeSavedInput>,
    kind: &'static str, participants: Vec<NativeRequestOwner>, cancel: CancelToken, pending: CancelToken,
    additional: Vec<CancelToken>, effects: OwnedEffectsHost, accepted: shell_contract::hosts::PdfReloadAccepted,
    wake: shell_contract::hosts::GeneratedPathWake,
) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String> {
    let mut result = None;
    let available = on_core_services_local(|provided, _| {
        result = Some(match provided.all::<TabsState>().into_iter().next() {
            Some(tabs) => tabs.prepare_generated_output(output, written, kind, participants, cancel,
                pending, additional, effects, accepted, wake)
                .map(|prepared| Box::new(prepared) as Box<dyn shell_contract::hosts::PreparedGeneratedPath>),
            None => Err("The tab manager is unavailable".into()),
        });
    });
    if !available { return Err("The Sheets core services are unavailable".into()); }
    result.ok_or_else(|| "The generated output was not prepared".to_owned())?
}

impl SheetsShell for Shell {
    fn workbook_opened(&self, view: ViewId, path: &str) {
        if crate::headless::is_headless() { return; }
        let path = path.to_owned();
        post_core(move |provided, _| {
            if let Some(tabs) = provided.all::<TabsState>().into_iter().next() {
                tabs.file_saved(view, &path);
            }
        });
    }

    fn workbook_opened_owned(&self, view: ViewId, path: String, origin: NativeRequestOwner, destination: NativeRequestOwner, cancel: CancelToken, pending: CancelToken, then: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>) {
        if crate::headless::is_headless() {
            return then(Err("Headless workbooks do not publish interactive saved sessions".into()));
        }
        if destination.view != view { return then(Err("The workbook destination does not match the opened view".into())); }
        shells::file_saved_owned(origin, destination, cancel, pending, self.effects.clone(), PathBuf::from(path), false, Box::new(move |result| {
            then(result.and_then(|saved| {
                let shell_contract::hosts::SavedFile { path, session } = saved;
                path.into_os_string().into_string()
                    .map(|path| shell_contract::hosts::SavedFile { path, session })
                    .map_err(|_| "The saved workbook path is not Unicode".to_owned())
            }));
        }));
    }

    fn workbook_opened_existing_document(
        &self, revision: shell_contract::ipc::NativeSaveRevisionAuthority, expected_path: PathBuf,
        participants: Vec<NativeRequestOwner>, cancel: CancelToken, pending: CancelToken,
        additional: Vec<CancelToken>, effects: OwnedEffectsHost,
        on_revision: std::rc::Rc<dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt) -> Result<(), String>>,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<PathBuf>, String>)>,
    ) {
        if crate::headless::is_headless() {
            return done(Err("Headless workbooks do not publish interactive saved sessions".into()));
        }
        shells::file_saved_existing_document(revision, expected_path, participants, cancel, pending,
            additional, effects, false, on_revision, done);
    }

    fn activate_tab(&self, view: ViewId) {
        if crate::headless::is_headless() { return; }
        post_core(move |provided, _| {
            if let Some(tabs) = provided.all::<TabsState>().into_iter().next()
                && let Some(id) = tabs.tab_id_for(view)
            { tabs.reveal_tab(&id); }
        });
    }

    fn password_prompt_owned(
        &self, parent: Option<WindowId>, file_name: &str, origin: &NativeRequestOwner,
        destination: &NativeRequestOwner, cancel: &CancelToken, pending: &CancelToken,
    ) -> Result<Option<Box<dyn PasswordPrompt>>, String> {
        self.passwords.create_owned(parent, file_name.to_owned(), origin.clone(), destination.clone(), cancel.clone(), pending.clone())
            .map(|prompt| Some(Box::new(Prompt(prompt)) as Box<dyn PasswordPrompt>))
    }
    fn is_headless(&self) -> bool { crate::headless::is_headless() }
    fn new_document_dir(&self, view: ViewId) -> Option<String> {
        assert!(!self.cx.ui.is_current() && !crate::host::w::on_core_local(|_| {}), "Workbook project resolution requires a worker");
        let (_, lookup, io) = on_core(move |provided| {
            let projects = provided.all::<shell_ipc_w::projects::Projects>().into_iter().next()?;
            Some(projects.capture_new_document(view))
        }).flatten()?;
        match io.resolve(lookup) {
            Ok(authority) => authority.map(|authority| authority.root().to_owned()),
            Err(error) => { eprintln!("[sheets] cannot resolve the project directory: {}", error.message()); None }
        }
    }

    fn write_new_document_owned(&self, origin: &NativeRequestOwner, destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        additional: &[CancelToken], effects: &OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        name: &str, bytes: &[u8]) -> Result<Option<(shell_contract::ipc::SavedPublication, shell_contract::ipc::NativeSavedInput)>, String> {
        crate::host::w_projects::write_new_document_publication_owned(origin, destination, participants,
            cancel, pending, additional, effects, admission, name, bytes)
    }

    fn create_document_owned(&self, request: IpcValue, view: ViewId, origin: NativeRequestOwner, destination: NativeRequestOwner, cancel: CancelToken, pending: CancelToken, _effects: OwnedEffectsHost, admission: main_core::packages::electron_utils::atomic_write::owned::Admission, then: Box<dyn FnOnce(Result<CreateDocumentAnswer, String>) + Send>) {
        let completion = Arc::new(std::sync::Mutex::new(Some(then)));
        let unavailable = completion.clone();
        crate::host::w::post_core_checked(move |provided, _| {
            let then = completion.lock().unwrap_or_else(|error| error.into_inner()).take();
            let Some(then) = then else { return; };
            let Some(docs) = provided.all::<shell_ipc_d2::docs::editors::DocsEditors>().into_iter().next() else {
                return then(Err("The document editor is unavailable".into()));
            };
            docs.create_ai_document_owned(request, view, origin, destination, cancel, pending, admission, then);
        }, move || {
            let then = unavailable.lock().unwrap_or_else(|error| error.into_inner()).take();
            if let Some(then) = then { then(Err("The Sheets core services are unavailable".into())); }
        });
    }

    fn open_generated_path(&self, path: &str) -> bool {
        let path = path.to_owned();
        on_core(move |provided| provided.all::<TabsState>().into_iter().next().is_some_and(|tabs| tabs.open_document_path(&path))).unwrap_or(false)
    }

    fn open_generated_path_owned(&self, output: shell_contract::ipc::SavedPublication,
        written: Option<shell_contract::ipc::NativeSavedInput>, participants: Vec<NativeRequestOwner>,
        cancel: CancelToken, pending: CancelToken, additional: Vec<CancelToken>, effects: OwnedEffectsHost,
        accepted: shell_contract::hosts::PdfReloadAccepted, wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String> {
        prepare_publication(output, written, "generated", participants, cancel, pending, additional, effects, accepted, wake)
    }

    fn export_pdf_written_owned(&self, output: shell_contract::ipc::SavedPublication,
        written: Option<shell_contract::ipc::NativeSavedInput>, participants: Vec<NativeRequestOwner>,
        cancel: CancelToken, pending: CancelToken, additional: Vec<CancelToken>, effects: OwnedEffectsHost,
        accepted: shell_contract::hosts::PdfReloadAccepted, wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String> {
        prepare_publication(output, written, "pdf", participants, cancel, pending, additional, effects, accepted, wake)
    }

    fn assert_file_saved(&self, path: String, then: Box<dyn FnOnce(Result<(), String>) + Send>) { super::save_guard::check(path, then); }
    fn is_path_open_elsewhere(&self, path: &str) -> bool { shells::open_elsewhere(path) }

    fn short_date_pattern(&self) -> Result<String, String> {
        let locale = crate::host::system_info::system_locale()?;
        seu_host::call("locale.shortDate", &serde_json::Value::String(locale)).map_err(|error| error.message)?
            .as_str().map(str::to_owned).ok_or_else(|| "The short date formatter returned a non-string".into())
    }

    fn requested_locale(&self) -> String {
        assert!(!self.cx.ui.is_current() && !crate::host::w::on_core_local(|_| {}), "Workbook locale resolution requires a worker");
        let (state, host) = on_core(|_| {
            let mut result = None;
            on_core_services_local(|_, services| {
                let state = services.hosts.get::<shell_ipc_m::state::MStateRef>().expect("The process language state is installed").clone();
                let host = services.hosts.get::<shell_ipc_m::hosts::MHostRef>().expect("The process locale host is installed").clone();
                result = Some((state, host));
            });
            result.expect("The process locale is read on the owning core thread")
        }).expect("The process locale is read after core registration");
        state.language(host.as_ref(), &crate::paths::ShellPaths::new().settings_file().to_string_lossy()).requested
    }

    fn delimited_import_limit(&self) -> f64 { shells::delimited_import_limit() }
    fn now_ms(&self) -> f64 { main_core::packages::pdf_structure::files::system_now_ms() }
    fn random_uuid(&self) -> String { jscompat::random_uuid() }
    fn app_version(&self) -> String { self.cx.app.package_info().version.to_string() }
    fn after(&self, ms: u64, job: Box<dyn FnOnce() + Send>) {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            post_core(move |_, _| job());
        });
    }
    fn template_fonts_dir(&self) -> String { self.fonts.clone() }
}
