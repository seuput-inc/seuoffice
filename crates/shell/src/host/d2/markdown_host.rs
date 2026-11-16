//! Markdown's hidden print surface.

use std::sync::{Arc, Mutex};

use main_core::packages::electron_utils::print_window::{JsError, RendererAnswer, RendererCall};
use shell_ipc_d2::markdown::MarkdownPrintWindow;
use shell_ipc_m::hosts::{PrintProfile, PrintSurface, PrintWindowSpec};

use crate::host::HostContext;

use main_core::markdown::image_host::{FetchFailure, FetchInit, FetchResponse};
use main_core::packages::electron_utils::safe_remote_url::{Fetcher, Resolver, Response};
use shell_ipc_d2::html::OdtConversion;
use shell_ipc_d2::markdown::{MarkdownEngine, MarkdownEngineHost};

pub(super) fn engine(cx: &HostContext) -> MarkdownEngineHost {
    Arc::new(Engine(cx.clone()))
}

struct Engine(HostContext);

impl MarkdownEngine for Engine {
    fn print_window(&self) -> Box<dyn MarkdownPrintWindow> {
        print_window(&self.0)
    }
    fn print_window_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn MarkdownPrintWindow>, String> {
        let surface = crate::host::print::markdown_window_owned(
            &self.0,
            origin,
            destination,
            cancel,
            pending,
        )?;
        Ok(Box::new(Window {
            surface: Arc::new(Mutex::new(surface)),
        }))
    }
    fn print_window_native(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
    ) -> Result<Box<dyn MarkdownPrintWindow>, String> {
        let surface = crate::host::print::markdown_window_existing_document(
            &self.0,
            binding,
            participants,
            cancel,
            pending,
            additional,
        )?;
        Ok(Box::new(Window {
            surface: Arc::new(Mutex::new(surface)),
        }))
    }
    fn print_window_invocation(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn MarkdownPrintWindow>, String> {
        let surface = crate::host::print::markdown_window_native_invocation(
            &self.0, document, owner, cancel, pending,
        )?;
        Ok(Box::new(Window {
            surface: Arc::new(Mutex::new(surface)),
        }))
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
    fn resolver(&self) -> Box<dyn Resolver + Send> {
        crate::host::net::ai::fetch::guarded_resolver()
    }
    fn fetcher(&self) -> Box<dyn Fetcher + Send> {
        crate::host::net::ai::fetch::guarded_fetcher()
    }
    fn net_fetch(
        &self,
        view: ViewId,
        url: &str,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn Response>, String> {
        if cancel.is_cancelled() {
            return Err("The operation was cancelled".into());
        }
        let parsed = tauri::Url::parse(url).map_err(|error| error.to_string())?;
        if parsed.scheme() == "md-asset"
            || (parsed.scheme() == "http" && parsed.host_str() == Some("md-asset.localhost"))
        {
            let asset = crate::host::document_assets::handle_cancellable(
                view,
                crate::host::document_assets::AssetKind::Markdown,
                url,
                None,
                None,
                cancel,
            )?;
            if cancel.is_cancelled() {
                return Err("The operation was cancelled".into());
            }
            return Ok(Box::new(
                main_core::packages::electron_utils::remote_image::MemoryResponse::new(
                    asset.status,
                    asset.headers,
                    asset.body,
                ),
            ));
        }
        let response = crate::host::net::ai::fetch::net_fetch_cancellable(
            url,
            crate::paths::resources_path(&self.0.app).as_ref(),
            cancel,
        )?;
        if cancel.is_cancelled() {
            return Err("The operation was cancelled".into());
        }
        Ok(response as Box<dyn Response>)
    }
    fn upload_fetch(&self, url: &str, init: &FetchInit<'_>) -> Result<FetchResponse, FetchFailure> {
        crate::host::net::ai::fetch::upload_fetch(url, init)
    }
}
use shell_contract::ViewId;
use shell_ipc_d2::markdown::{MarkdownShell, MarkdownShellHost};
use shell_ipc_w::editor_files::UiLanguage;
use shell_ipc_w::tabs::TabsState;

pub(super) fn shell(
    cx: &HostContext,
    hosts: &shell_contract::Hosts,
) -> Result<MarkdownShellHost, String> {
    let state = hosts
        .get::<shell_ipc_w::SharedSaveDirState>()
        .ok_or("save directory state unavailable")?
        .clone();
    crate::host::document_assets::set_vault_resolver(Arc::new(move || {
        let mut state = state.lock().map_err(|_| "save directory state poisoned")?;
        main_core::packages::electron_utils::default_save_dir::configured_default_save_dir(
            &crate::paths::ShellPaths::new(),
            &mut state,
            main_core::packages::pdf_structure::files::system_now_ms(),
        )
        .map(Some)
        .map_err(|error| error.0)
    }))?;
    Ok(Arc::new(Shell(cx.clone())))
}

struct Shell(HostContext);

impl MarkdownShell for Shell {
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
        crate::host::w::on_core(move |provided| {
            if let Some(tabs) = crate::host::shell_tabs::shell_tabs(provided) {
                tabs.reload_view(view);
            }
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
    fn open_generated_path(&self, path: &std::path::Path) -> bool {
        super::docs_tabs::open_generated(&path.to_string_lossy()).unwrap_or(false)
    }
    fn new_document_dir(&self, view: ViewId) -> Option<std::path::PathBuf> {
        crate::host::d1::shells::new_document_dir(view).map(Into::into)
    }
    fn write_new_document(
        &self,
        view: ViewId,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<std::path::PathBuf>, String> {
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
    ) -> Result<Option<std::path::PathBuf>, String> {
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
    ) -> Result<Option<std::path::PathBuf>, String> {
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
            document_assets::set_paths(view, AssetKind::Markdown, open, save)
        } else {
            document_assets::revoke_kind(view, AssetKind::Markdown)
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
            crate::host::document_assets::AssetKind::Markdown,
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
            crate::host::document_assets::AssetKind::Markdown,
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
            crate::host::document_assets::AssetKind::Markdown,
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
            crate::host::document_assets::AssetKind::Markdown,
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
            crate::host::document_assets::AssetKind::Markdown,
            tracked,
            open,
            save,
        )
    }
    fn file_saved(&self, view: ViewId, path: &std::path::Path) {
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
        path: std::path::PathBuf,
        then: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>,
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
        then: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>,
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
    fn docx_exported(&self, path: &std::path::Path) {
        let _ = super::docs_tabs::open_generated(&path.to_string_lossy());
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
        super::html_host::prepare_generated_output(
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
}
pub(super) fn print_window(cx: &HostContext) -> Box<dyn MarkdownPrintWindow> {
    let surface = crate::host::print::window(
        cx.app.clone(),
        cx.ui.clone(),
        cx.views.clone(),
        PrintWindowSpec {
            profile: PrintProfile::Static,
            width: None,
            height: None,
            parent: None,
            dialog_chrome: false,
        },
    );
    Box::new(Window {
        surface: Arc::new(Mutex::new(surface)),
    })
}

struct Window {
    surface: Arc<Mutex<Box<dyn PrintSurface>>>,
}

impl MarkdownPrintWindow for Window {
    fn call(&mut self, call: RendererCall, answer: Box<dyn FnOnce(RendererAnswer) + Send>) {
        let surface = Arc::clone(&self.surface);
        std::thread::spawn(move || {
            let result = {
                let mut surface = surface.lock().unwrap_or_else(|e| e.into_inner());
                match call {
                    RendererCall::LoadFile { path } => {
                        RendererAnswer::Loaded(surface.load_file(&path).map_err(JsError::plain))
                    }
                    RendererCall::ExecuteJavaScript {
                        script,
                        user_gesture,
                        race_ms,
                    } => RendererAnswer::Executed(surface.execute_javascript(
                        script,
                        user_gesture,
                        race_ms,
                    )),
                    RendererCall::PrintToPdf { options } => {
                        RendererAnswer::Pdf(surface.print_to_pdf(&options).map_err(JsError::plain))
                    }
                    RendererCall::Print { options } => {
                        RendererAnswer::Printed(Ok(surface.print(&options)))
                    }
                }
            };
            answer(result);
        });
    }

    fn destroy(&mut self) -> Result<(), JsError> {
        self.surface
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .destroy();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use main_core::packages::electron_utils::print_window::{ExecOutcome, PrintReport};
    use serde_json::Value;
    use std::sync::mpsc;

    struct Surface;
    impl PrintSurface for Surface {
        fn load_file(&mut self, path: &str) -> Result<(), String> {
            if path == "synthetic.html" {
                Ok(())
            } else {
                Err("missing".into())
            }
        }
        fn load_url(&mut self, _: &str) -> Result<(), String> {
            Err("unexpected".into())
        }
        fn execute_javascript(&mut self, _: &str, _: bool, _: Option<f64>) -> ExecOutcome {
            ExecOutcome::Pending
        }
        fn print(&mut self, _: &Value) -> PrintReport {
            PrintReport {
                success: true,
                failure_reason: String::new(),
            }
        }
        fn print_to_pdf(&mut self, _: &Value) -> Result<Vec<u8>, String> {
            Ok(b"%PDF-synthetic".to_vec())
        }
        fn show(&mut self) {}
        fn focus(&mut self) {}
        fn is_destroyed(&self) -> bool {
            false
        }
        fn destroy(&mut self) {}
    }

    #[test]
    fn renderer_replies_with_load_errors_and_pdf_bytes() {
        let mut window = Window {
            surface: Arc::new(Mutex::new(Box::new(Surface))),
        };
        let (tx, rx) = mpsc::channel();
        window.call(
            RendererCall::LoadFile {
                path: "missing.html".into(),
            },
            Box::new(move |v| tx.send(v).unwrap()),
        );
        assert_eq!(
            rx.recv().unwrap(),
            RendererAnswer::Loaded(Err(JsError::plain("missing")))
        );
        let (tx, rx) = mpsc::channel();
        window.call(
            RendererCall::PrintToPdf {
                options: serde_json::json!({}),
            },
            Box::new(move |v| tx.send(v).unwrap()),
        );
        assert_eq!(
            rx.recv().unwrap(),
            RendererAnswer::Pdf(Ok(b"%PDF-synthetic".to_vec()))
        );
    }
}
