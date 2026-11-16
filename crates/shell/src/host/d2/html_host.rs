//! Native HTML editor helpers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use shell_ipc_d2::html::ParsedFile;

use crate::host::HostContext;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use main_core::packages::electron_utils::generate_handoff::GenerateHandoffTarget;
use main_core::packages::electron_utils::safe_remote_url::{Fetcher, Resolver};
use shell_contract::IpcValue;
use shell_contract::runtime::Views;
use shell_contract::{ViewId, WindowId};
use shell_ipc_d2::html::{Handoff, HtmlShell, HtmlShellHost};
use shell_ipc_d2::html::{HtmlEngine, HtmlEngineHost, OdtConversion};
use shell_ipc_w::editor_files::UiLanguage;
use shell_ipc_w::tabs::TabsState;
use std::time::Duration;
use tauri::Manager;

pub(super) fn engine(cx: &HostContext) -> HtmlEngineHost {
    Arc::new(Engine(cx.clone()))
}

struct Engine(HostContext);

impl HtmlEngine for Engine {
    fn print_pdf(
        &self,
        view: ViewId,
        html: &str,
        doc_path: Option<&str>,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Vec<u8>, String> {
        crate::host::print::render_html_pdf(&self.0, view, html, doc_path, cancel)
    }
    fn print_pdf_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        html: &str,
        doc_path: Option<&str>,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Vec<u8>, String> {
        crate::host::print::render_html_pdf_owned(
            &self.0,
            origin,
            destination,
            html,
            doc_path,
            cancel,
            pending,
        )
    }
    fn print_pdf_native(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        html: &str,
        doc_path: Option<&str>,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
    ) -> Result<Vec<u8>, String> {
        crate::host::print::render_html_pdf_existing_document(
            &self.0,
            binding,
            participants,
            html,
            doc_path,
            cancel,
            pending,
            additional,
        )
    }
    fn print_pdf_invocation(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        owner: &shell_contract::ipc::NativeRequestOwner,
        html: &str,
        doc_path: Option<&str>,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Vec<u8>, String> {
        crate::host::print::render_html_pdf_native_invocation(
            &self.0, document, owner, html, doc_path, cancel, pending,
        )
    }
    fn html_to_docx(
        &self,
        view: ViewId,
        path: &Path,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Vec<u8>, String> {
        let path = path.to_str().ok_or("HTML export path is not UTF-8")?;
        let reply = crate::engine_host::call_with_timeout_cancellable(
            "docs.htmlToDocx",
            &serde_json::json!({ "path": path, "naturalTableWidth": false, "ownerViewId": view.0 }),
            Duration::from_secs(180),
            cancel,
        )
        .map_err(|error| error.message)?;
        decode_docx(reply)
    }
    fn html_to_docx_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        path: &Path,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Vec<u8>, String> {
        let path = path.to_str().ok_or("HTML export path is not UTF-8")?;
        let reply=crate::engine_host::call_owned_pair("docs.htmlToDocx",&serde_json::json!({"path":path,"naturalTableWidth":false,"ownerViewId":destination.view.0}),origin,destination,cancel,pending).map_err(|error|error.message)?;
        decode_docx(reply)
    }
    fn html_to_docx_native(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        path: &Path,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
    ) -> Result<Vec<u8>, String> {
        let path = path.to_str().ok_or("HTML export path is not UTF-8")?;
        let reply=crate::engine_host::call_existing_document_save_with_timeout("docs.htmlToDocx",&serde_json::json!({"path":path,"naturalTableWidth":false,"ownerViewId":binding.owner().view.0}),binding,participants,cancel,pending,additional,Duration::from_secs(180)).map_err(|error|error.message)?;
        decode_docx(reply)
    }
    fn html_to_docx_invocation(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        owner: &shell_contract::ipc::NativeRequestOwner,
        path: &Path,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Vec<u8>, String> {
        let path = path.to_str().ok_or("HTML export path is not UTF-8")?;
        let reply = crate::engine_host::call_native_invocation_with_timeout(
            "docs.htmlToDocx",
            &serde_json::json!({"path":path,"naturalTableWidth":false,"ownerViewId":owner.view.0}),
            document.target(),
            owner,
            cancel,
            pending,
            Duration::from_secs(180),
        )
        .map_err(|error| error.message)?;
        decode_docx(reply)
    }
    fn docx_to_odt_cancellable(
        &self,
        docx: Vec<u8>,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<OdtConversion, String> {
        super::odt_export::convert_cancellable(&docx, cancel)
    }
    fn docx_to_odt(&self, docx: Vec<u8>) -> Result<OdtConversion, String> {
        super::odt_export::convert(&docx)
    }
    fn parse_file_to_text(&self, path: &Path) -> ParsedFile {
        parse_file(path)
    }
    fn diagram_picture_ext_of(&self, path: &Path) -> Option<String> {
        diagram_picture_ext(path)
    }
    fn resolver(&self) -> Box<dyn Resolver + Send> {
        crate::host::net::ai::fetch::guarded_resolver()
    }
    fn fetcher(&self) -> Box<dyn Fetcher + Send> {
        crate::host::net::ai::fetch::guarded_fetcher()
    }
}

fn decode_docx(reply: serde_json::Value) -> Result<Vec<u8>, String> {
    let invalid = || "HTML to DOCX conversion returned invalid bytes".to_owned();
    let header = reply.get("header").ok_or_else(invalid)?;
    let buffers = reply
        .get("buffers")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(invalid)?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(invalid)
                .and_then(|text| STANDARD.decode(text).map_err(|_| invalid()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    match shell_core::wire::decode(header, &buffers).map_err(|_| invalid())? {
        IpcValue::Bytes(_, bytes) => Ok(bytes),
        _ => Err(invalid()),
    }
}

pub(super) fn shell(cx: &HostContext) -> HtmlShellHost {
    Arc::new(Shell(cx.clone()))
}

struct Shell(HostContext);

impl HtmlShell for Shell {
    fn is_headless(&self) -> bool {
        crate::headless::is_headless()
    }
    fn ui_lang(&self) -> String {
        crate::host::w_files::ShellLanguage(self.0.ui.clone())
            .lang()
            .as_str()
            .to_owned()
    }
    fn now_ms(&self) -> f64 {
        main_core::packages::pdf_structure::files::system_now_ms()
    }
    fn reload(&self, view: ViewId) {
        let cx = self.0.clone();
        crate::host::on_ui(&self.0.ui, move || {
            cx.windows.reload(view);
        });
    }
    fn is_path_open_elsewhere(&self, path: &str) -> bool {
        let path = path.to_owned();
        crate::host::w::on_core(move |provided| {
            provided
                .all::<TabsState>()
                .into_iter()
                .next()
                .map(|tabs| tabs.is_path_open_elsewhere(&path))
        })
        .flatten()
        .unwrap_or(true)
    }
    fn assert_file_saved(&self, path: &str, then: Box<dyn FnOnce(Result<(), String>)>) {
        crate::host::d1::save_guard::check_local(path.to_owned(), then);
    }
    fn open_generated_path(&self, path: &Path) -> bool {
        super::docs_tabs::open_generated(&path.to_string_lossy()).unwrap_or(false)
    }
    fn new_document_dir(&self, view: ViewId) -> Option<PathBuf> {
        crate::host::d1::shells::new_document_dir(view).map(Into::into)
    }
    fn write_new_document(
        &self,
        view: ViewId,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<PathBuf>, String> {
        crate::host::d1::shells::write_new_document(view, file_name, bytes)
            .map(|path| path.map(Into::into))
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
    ) -> Result<Option<PathBuf>, String> {
        crate::host::w_projects::write_new_document_owned(
            origin,
            destination,
            cancel,
            pending,
            effects,
            admission,
            file_name,
            bytes,
        )
        .map(|path| path.map(Into::into))
    }
    #[allow(clippy::too_many_arguments)]
    fn write_new_document_existing_document(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<PathBuf>, String> {
        crate::host::w_projects::write_new_document_existing_document(
            binding,
            participants,
            cancel,
            pending,
            additional,
            effects,
            admission,
            file_name,
            bytes,
        )
        .map(|path| path.map(Into::into))
    }
    fn asset_paths_changed(
        &self,
        view: ViewId,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<(), String> {
        use crate::host::document_assets::{self, AssetKind};
        if tracked {
            document_assets::set_paths(view, AssetKind::Html, open, save)
        } else {
            document_assets::revoke_kind(view, AssetKind::Html)
        }
    }
    fn asset_paths_changed_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        captured: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<(), String> {
        crate::host::document_assets::set_paths_many_owned(
            origin,
            captured,
            cancel,
            pending,
            additional,
            effects,
            crate::host::document_assets::AssetKind::Html,
            tracked,
            open,
            save,
        )
    }
    fn prepare_native_save_asset_paths(
        &self,
        destination: &shell_contract::ipc::NativeRequestOwner,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<shell_ipc_d2::html::RecoveryAssetStage, String> {
        crate::host::document_assets::prepare_native_save_paths(
            destination,
            crate::host::document_assets::AssetKind::Html,
            tracked,
            open,
            save,
        )
    }
    fn prepare_saved_document_asset_paths(
        &self,
        destination: &shell_contract::ipc::NativeRequestOwner,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<shell_ipc_d2::html::SavedAssetStage, String> {
        crate::host::document_assets::prepare_saved_document_paths(
            destination,
            crate::host::document_assets::AssetKind::Html,
            open,
            save,
        )
    }
    fn prepare_recovery_asset_paths(
        &self,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<Box<dyn FnMut() -> Result<(), String> + Send>, String> {
        crate::host::document_assets::prepare_recovery_paths(
            target,
            crate::host::document_assets::AssetKind::Html,
            tracked,
            open,
            save,
        )
    }
    fn prepare_rename_asset_paths_recovery(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        _participants: &[shell_contract::ipc::NativeRequestOwner],
        _cancel: &shell_contract::runtime::CancelToken,
        _pending: &shell_contract::runtime::CancelToken,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<shell_ipc_d2::html::RecoveryAssetStage, String> {
        crate::host::document_assets::prepare_rename_recovery_paths(
            receipt,
            receiver,
            destination,
            crate::host::document_assets::AssetKind::Html,
            tracked,
            open,
            save,
        )
    }
    fn asset_present_owner(&self, view: ViewId, owner: Option<ViewId>) -> Result<(), String> {
        crate::host::document_assets::set_present_owner(view, owner)
    }
    fn file_saved(&self, view: ViewId, path: &Path) {
        let path = path.to_string_lossy().into_owned();
        crate::host::w::on_core(move |provided| {
            if let Some(tabs) = provided.all::<TabsState>().into_iter().next()
                && tabs.details_of(view).is_some()
            {
                tabs.file_saved(view, &path);
                crate::host::menu::note_document_saved(view, &path, false);
                tabs.apply_pending_dir(view, &path);
            }
        });
    }
    fn file_saved_owned(
        &self,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        path: PathBuf,
        then: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<PathBuf>, String>)>,
    ) {
        crate::host::d1::shells::file_saved_many_owned_extra(
            origin,
            destination,
            participants,
            cancel,
            pending,
            additional,
            effects,
            path,
            true,
            then,
        )
    }
    fn file_saved_native(
        &self,
        destination: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        then: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<PathBuf>, String>)>,
    ) {
        crate::host::d1::shells::file_saved_existing_document(
            destination,
            participants,
            cancel,
            pending,
            additional,
            effects,
            true,
            then,
        )
    }
    fn provisional_title(&self, view: ViewId, title: &str) {
        let title = title.to_owned();
        crate::host::w::on_core(move |provided| {
            if let Some(tabs) = provided.all::<TabsState>().into_iter().next() {
                tabs.set_document_title(view, &title);
            }
        });
    }
    fn docx_exported(&self, path: &Path) -> bool {
        let _ = super::docs_tabs::open_generated(&path.to_string_lossy());
        true
    }
    fn docx_export_prepare(&self, path: &Path, then: Box<dyn FnOnce(bool)>) {
        prepare_docx(path, then);
    }
    fn docx_export_prepare_owned(
        &self,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
        path: PathBuf,
        then: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        prepare_docx_owned(path, origin, destination, cancel, pending, effects, then)
    }
    fn docx_export_prepare_native(
        &self,
        selection: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        then: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        prepare_docx_native(
            selection,
            participants,
            cancel,
            pending,
            additional,
            effects,
            then,
        );
    }
    fn prepare_generated_output(
        &self,
        output: shell_contract::ipc::SavedPublication,
        input: Option<shell_contract::ipc::NativeSavedInput>,
        kind: &'static str,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        accepted: shell_contract::hosts::PdfReloadAccepted,
        wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String> {
        prepare_generated_output(
            output,
            input,
            kind,
            participants,
            cancel,
            pending,
            additional,
            effects,
            accepted,
            wake,
        )
    }
    fn register_generated_actor_cleanup(
        &self,
        wake: shell_ipc_d2::docs::hosts::GeneratedActorWake,
    ) -> Result<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>, String> {
        super::docs_shell::cleanup::register(&self.0.headless_cleanup, wake)
    }
    fn take_generate_handoff_owned(
        &self,
        consumer: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Option<Handoff>, String> {
        take_handoff(consumer, cancel)
    }
    fn report_generate_run_started_owned(
        &self,
        consumer: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        paths: &[String],
    ) -> Result<bool, String> {
        report_handoff(consumer, cancel, paths)
    }
    fn present_open_tab(&self, owner: ViewId, title: &str, then: Box<dyn FnOnce(bool)>) {
        open_present_tab(owner, title, then)
    }
    fn present_host_window(&self, view: ViewId) -> Option<WindowId> {
        self.0.views.sender_of(view).map(|sender| sender.window)
    }
    fn present_set_bleed(&self, view: ViewId, on: bool) {
        set_present_bleed(view, on);
    }
    fn present_enter_full_screen(&self, window: WindowId) {
        if let Err(error) = crate::host::w_tabs::set_html_simple_fullscreen(&self.0, window, true) {
            eprintln!("HTML present fullscreen: {error}");
        }
    }
    fn present_leave_full_screen(&self, window: WindowId) {
        if let Err(error) = crate::host::w_tabs::set_html_simple_fullscreen(&self.0, window, false)
        {
            eprintln!("HTML present fullscreen: {error}");
        }
    }
    fn is_window_destroyed(&self, window: WindowId) -> bool {
        self.0
            .views
            .label_of_window(window)
            .and_then(|label| self.0.app.get_window(&label))
            .is_none()
    }
    fn open_present_window(
        &self,
        owner: ViewId,
        title: &str,
        parent: Option<WindowId>,
    ) -> Option<ViewId> {
        let result = std::rc::Rc::new(std::cell::RefCell::new(None));
        let output = result.clone();
        let title = title.to_owned();
        let cx = self.0.clone();
        crate::host::w::on_core_local(move |provided| {
            *output.borrow_mut() =
                crate::host::w_tabs::open_html_present_window(&cx, provided, owner, &title, parent)
                    .map_err(|error| eprintln!("HTML present window: {error}"))
                    .ok();
        });
        let view = result.borrow_mut().take();
        view
    }
    fn close_present_view(&self, view: ViewId) {
        let cx = self.0.clone();
        crate::host::w::on_core_local(move |provided| {
            if let Err(error) = crate::host::w_tabs::close_html_present_view(&cx, provided, view) {
                eprintln!("HTML present close: {error}");
            }
        });
    }
}

fn take_handoff(
    consumer: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken,
) -> Result<Option<Handoff>, String> {
    let consumer = consumer.clone();
    let cancel = cancel.clone();
    crate::host::w::on_core(move |provided| {
        shell_ipc_a::user_skills::take_generate_handoff_owned(
            provided,
            GenerateHandoffTarget::Html,
            &consumer,
            &cancel,
        )
        .map(|handoff| {
            handoff.map(|handoff| Handoff {
                id: handoff.id,
                text: handoff.text,
                materials: handoff
                    .materials
                    .into_iter()
                    .map(|item| (item.path, item.name))
                    .collect(),
            })
        })
    })
    .ok_or_else(|| "Core services are not available".to_owned())?
}

fn report_handoff(
    consumer: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken,
    paths: &[String],
) -> Result<bool, String> {
    let consumer = consumer.clone();
    let cancel = cancel.clone();
    let paths = paths.to_vec();
    crate::host::w::on_core(move |provided| {
        shell_ipc_a::user_skills::report_generate_run_started_owned(
            provided,
            GenerateHandoffTarget::Html,
            &consumer,
            &cancel,
            &paths,
            main_core::packages::pdf_structure::files::system_now_ms(),
        )
        .map(|event| event.is_some())
    })
    .ok_or_else(|| "Core services are not available".to_owned())?
}

fn prepare_docx(path: &Path, then: Box<dyn FnOnce(bool)>) {
    if crate::headless::is_headless() {
        then(true);
        return;
    }
    let path = path.to_string_lossy().into_owned();
    let pending = std::rc::Rc::new(std::cell::RefCell::new(Some(then)));
    let callback = pending.clone();
    if !crate::host::w::on_core_local(move |provided| {
        let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
            if let Some(then) = callback.borrow_mut().take() {
                then(false);
            }
            return;
        };
        tabs.prepare_html_docx_export(
            &path,
            Box::new(move |result| {
                if let Some(then) = callback.borrow_mut().take() {
                    then(result.unwrap_or(false));
                }
            }),
        );
    }) {
        if let Some(then) = pending.borrow_mut().take() {
            then(false);
        }
    }
}

fn open_present_tab(owner: ViewId, title: &str, then: Box<dyn FnOnce(bool)>) {
    let pending = std::rc::Rc::new(std::cell::RefCell::new(Some(then)));
    let local = pending.clone();
    let title = title.to_owned();
    let started = crate::host::w::on_core_local(move |provided| {
        let Some(then) = local.borrow_mut().take() else {
            return;
        };
        let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
            return then(false);
        };
        tabs.open_html_present_owned(
            owner,
            title,
            Box::new(move |result| {
                if let Err(error) = &result {
                    eprintln!("shell: opening the present tab: {error}");
                }
                then(result.is_ok());
            }),
        );
    });
    if !started && let Some(then) = pending.borrow_mut().take() {
        then(false);
    }
}

fn set_present_bleed(view: ViewId, on: bool) {
    crate::host::w::on_core(move |provided| {
        if let Some(tabs) = provided.all::<TabsState>().into_iter().next() {
            tabs.with_manager(|manager, host| {
                manager.set_content_bleed(host, i64::from(view.0), on)
            });
        }
    });
}

pub(super) fn parse_file(path: &Path) -> ParsedFile {
    let path = path.to_string_lossy();
    match crate::host::w_files::parsed_value(&path) {
        Ok(value) => parsed_result(&value),
        Err(error) => ParsedFile {
            error: Some(error),
            ..ParsedFile::default()
        },
    }
}

fn parsed_result(value: &serde_json::Value) -> ParsedFile {
    match value.get("ok").and_then(|v| v.as_bool()) {
        Some(ok) => ParsedFile {
            ok,
            kind: value
                .get("kind")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            text: value
                .get("text")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            damaged: value
                .get("damaged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            error: value
                .get("error")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        },
        None => ParsedFile {
            error: Some("file parser returned no ok flag".into()),
            ..ParsedFile::default()
        },
    }
}

pub(super) fn diagram_picture_ext(path: &Path) -> Option<String> {
    crate::host::w_files::diagram_picture_ext(path).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_preserves_text_damage_and_failures() {
        let result = parsed_result(
            &serde_json::json!({ "ok": true, "kind": "text", "text": "synthetic", "damaged": true }),
        );
        assert!(result.ok);
        assert_eq!(result.text.as_deref(), Some("synthetic"));
        assert!(result.damaged);
        let result = parsed_result(&serde_json::json!({ "ok": false, "error": "bad file" }));
        assert!(!result.ok);
        assert_eq!(result.error.as_deref(), Some("bad file"));
        assert!(parsed_result(&serde_json::json!({})).error.is_some());
    }

    #[test]
    fn conversion_reply_requires_real_wire_bytes() {
        let response = serde_json::json!({
            "header": { "$": "b", "k": "u8", "i": 0 },
            "buffers": ["UEsDBA=="]
        });
        assert_eq!(decode_docx(response).unwrap(), b"PK\x03\x04");
        assert!(decode_docx(serde_json::json!({ "header": null, "buffers": [] })).is_err());
        assert!(
            decode_docx(serde_json::json!({
                "header": { "$": "b", "k": "u8", "i": 0 }, "buffers": ["!"]
            }))
            .is_err()
        );
    }
}

fn prepare_docx_owned(
    path: PathBuf,
    origin: shell_contract::ipc::NativeRequestOwner,
    destination: shell_contract::ipc::NativeRequestOwner,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    effects: shell_contract::hosts::OwnedEffectsHost,
    then: Box<dyn FnOnce(Result<bool, String>)>,
) {
    let callback = std::rc::Rc::new(std::cell::RefCell::new(Some(then)));
    let accepted = callback.clone();
    if !crate::host::w::on_core_local(move |provided| {
        let done = accepted
            .borrow_mut()
            .take()
            .expect("export callback is consumed once");
        let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
            return done(Err("Tabs services are not available".into()));
        };
        tabs.prepare_html_docx_export_owned(
            &path,
            origin,
            destination,
            cancel,
            pending,
            effects,
            done,
        );
    }) {
        if let Some(done) = callback.borrow_mut().take() {
            done(Err("Core services are not available".into()));
        }
    }
}

/// The stale Word tab of a native "convert and open in Docs" export. The save's
/// authority is checked before the tab is closed and again before the answer,
/// so a lost destination or cancelled save never reports the export ready.
#[allow(clippy::too_many_arguments)]
fn prepare_docx_native(
    selection: shell_contract::ipc::NativeSaveDestination,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    additional: Vec<shell_contract::runtime::CancelToken>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    then: Box<dyn FnOnce(Result<bool, String>)>,
) {
    let path = selection.path().to_string_lossy().into_owned();
    let admit = move || {
        let extra: Vec<_> = additional.iter().collect();
        effects.with_existing_document_destination_effect(
            &selection,
            selection.path(),
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || Ok(()),
        )
    };
    if let Err(error) = admit() {
        return then(Err(error));
    }
    let callback = std::rc::Rc::new(std::cell::RefCell::new(Some(then)));
    let accepted = callback.clone();
    if !crate::host::w::on_core_local(move |provided| {
        let Some(done) = accepted.borrow_mut().take() else {
            return;
        };
        let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
            return done(Err("Tabs services are not available".into()));
        };
        tabs.prepare_html_docx_export(
            &path,
            Box::new(move |ready| done(ready.and_then(|ready| admit().map(|()| ready)))),
        );
    }) {
        if let Some(done) = callback.borrow_mut().take() {
            done(Err("Core services are not available".into()));
        }
    }
}

/// Prepares the retained open or reveal of a published export on the core
/// thread, where it was published.
#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_generated_output(
    output: shell_contract::ipc::SavedPublication,
    input: Option<shell_contract::ipc::NativeSavedInput>,
    kind: &'static str,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    additional: Vec<shell_contract::runtime::CancelToken>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    accepted: shell_contract::hosts::PdfReloadAccepted,
    wake: shell_contract::hosts::GeneratedPathWake,
) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String> {
    let mut result = None;
    let on_core = crate::host::w::on_core_local(|provided| {
        result = Some(match provided.all::<TabsState>().into_iter().next() {
            Some(tabs) => tabs
                .prepare_generated_output(
                    output,
                    input,
                    kind,
                    participants,
                    cancel,
                    pending,
                    additional,
                    effects,
                    accepted,
                    wake,
                )
                .map(|prepared| {
                    Box::new(prepared) as Box<dyn shell_contract::hosts::PreparedGeneratedPath>
                }),
            None => Err("The tab manager is unavailable".into()),
        });
    });
    if !on_core {
        return Err("The exported document must be prepared on the core thread".into());
    }
    result.ok_or_else(|| "The tab manager did not prepare the exported document".to_owned())?
}
