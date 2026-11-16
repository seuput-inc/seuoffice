//! Shell services used by the Word editor.
pub(super) mod cleanup;

use main_core::packages::electron_utils::safe_remote_url::Response;
use main_core::packages::i18n::Lang;
use shell_contract::ViewId;
use shell_ipc_d2::docs::hosts::{DocsShell, DocsShellHost};
use shell_ipc_w::editor_files::UiLanguage;
use std::sync::Arc;

use super::{docs_image, docs_print, docs_tabs};
use crate::host::{HostContext, d1::shells, menu, w_files::ShellLanguage};

pub(super) fn host(
    cx: &HostContext,
    hosts: &shell_contract::Hosts,
) -> Result<DocsShellHost, String> {
    let passwords = hosts
        .get::<Arc<crate::host::document_password::DocumentPasswordService>>()
        .cloned()
        .ok_or("The shared document password service is unavailable")?;
    Ok(Arc::new(Shell {
        cx: cx.clone(),
        language: ShellLanguage(cx.ui.clone()),
        passwords,
    }))
}

struct Shell {
    cx: HostContext,
    language: ShellLanguage,
    passwords: Arc<crate::host::document_password::DocumentPasswordService>,
}

impl DocsShell for Shell {
    fn register_generated_actor_cleanup(
        &self,
        wake: shell_ipc_d2::docs::hosts::GeneratedActorWake,
    ) -> Result<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>, String> {
        cleanup::register(&self.cx.headless_cleanup, wake)
    }
    fn unlock_recovery_prepared(
        &self,
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        title: String,
        bytes: &[u8],
    ) -> Result<Option<(Vec<u8>, String)>, String> {
        if self.cx.ui.is_current() || crate::host::w::on_core_local(|_| {}) {
            return Err("Recovery decryption requires a worker".into());
        }
        crate::host::document_password::unlock_prepared(
            &self.passwords,
            target,
            &snapshot,
            title,
            bytes,
        )
    }
    fn unlock_saved_document(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        title: String,
    ) -> Result<Option<(Vec<u8>, String)>, String> {
        if self.cx.ui.is_current() || crate::host::w::on_core_local(|_| {}) {
            return Err("Saved document decryption requires a worker".into());
        }
        crate::host::document_password::unlock_saved(&self.passwords, document, title)
    }
    fn capture_dirty_query_target(
        &self,
        view: ViewId,
        expected: &shell_contract::runtime::RendererGeneration,
    ) -> Result<shell_contract::ipc::NativeRequestOwner, String> {
        let owner = crate::ipc::native_owner::capture_owner(&self.cx.views, view, expected)?;
        if owner
            .context
            .downcast_ref::<main_core::docs::byte_handoff::MediaViewLease>()
            .is_none()
        {
            return Err("the dirty-query target is not a Docs document".into());
        }
        Ok(owner)
    }
    fn ui_lang(&self) -> Lang {
        self.language.lang()
    }
    fn is_headless(&self) -> bool {
        crate::headless::is_headless()
    }
    fn random_uuid(&self) -> String {
        jscompat::random_uuid()
    }
    fn now_ms(&self) -> f64 {
        main_core::packages::pdf_structure::files::system_now_ms()
    }
    fn file_saved(&self, view: ViewId, path: &str) -> Option<String> {
        docs_tabs::file_saved(view, path)
    }
    fn file_saved_owned(
        &self,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
        path: std::path::PathBuf,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>,
    ) {
        shells::file_saved_many_owned(
            origin,
            destination,
            participants,
            cancel,
            pending,
            effects,
            path,
            false,
            done,
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
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>,
    ) {
        shells::file_saved_existing_document(
            destination,
            participants,
            cancel,
            pending,
            additional,
            effects,
            false,
            done,
        );
    }
    fn prepare_docs_menu_refresh_existing_document(
        &self,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_d2::docs::hosts::OwnedMenuRefresh>, String> {
        menu::prepare_docs_menu_refresh_existing_document(
            &binding,
            &participants,
            &cancel,
            &pending,
            &additional,
            &effects,
        )
    }
    fn file_opened(&self, view: ViewId, path: &str) {
        if !crate::headless::is_headless() {
            docs_tabs::file_opened(view, path);
        }
    }
    fn file_opened_recovery(
        &self,
        path: String,
        receipt: shell_contract::ipc::RecoverySeedReceipt,
        receiver: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        if crate::headless::is_headless() {
            return done(effects.with_recovery_open_effect_pending(
                &receipt,
                &receiver,
                &cancel,
                &pending,
                &mut || Ok(()),
            ));
        }
        if !crate::host::w::on_core_local(|_| {}) {
            return done(Err(
                "Recovery file publication requires the core thread".into()
            ));
        }
        crate::host::w_home::docs_file_opened_recovery(
            path, receipt, receiver, cancel, pending, effects, done,
        );
    }
    fn new_document_dir(&self, view: ViewId) -> Option<String> {
        shells::new_document_dir(view)
    }
    fn writes_new_documents(&self) -> bool {
        true
    }
    fn write_new_document(
        &self,
        view: ViewId,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> Option<Result<Option<String>, String>> {
        Some(shells::write_new_document(view, file_name, &bytes))
    }
    fn write_new_document_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        name: &str,
        bytes: &[u8],
    ) -> Result<
        Option<(
            shell_contract::ipc::SavedPublication,
            shell_contract::ipc::NativeSavedInput,
        )>,
        String,
    > {
        crate::host::w_projects::write_new_document_publication_owned(
            origin,
            destination,
            participants,
            cancel,
            pending,
            additional,
            effects,
            admission,
            name,
            bytes,
        )
    }
    fn write_new_document_existing_document(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        name: &str,
        bytes: &[u8],
    ) -> Result<Option<String>, String> {
        crate::host::w_projects::write_new_document_existing_document(
            binding,
            participants,
            cancel,
            pending,
            additional,
            effects,
            admission,
            name,
            bytes,
        )
    }
    fn permits_synthetic_create(&self) -> bool {
        false
    }
    fn open_ai_doc_tab_synthetic(
        &self,
        _content: serde_json::Value,
        _owner: ViewId,
    ) -> Result<(), String> {
        Err("Synthetic AI document creation is unavailable in the native host".into())
    }
    fn open_ai_doc_tab_owned(
        &self,
        content: serde_json::Value,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        _effects: shell_contract::hosts::OwnedEffectsHost,
        then: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        docs_tabs::open_ai_owned(content, origin, destination, cancel, pending, then);
    }
    fn open_generated_path(&self, path: &str) -> Result<bool, String> {
        docs_tabs::open_generated(path)
    }
    fn prepare_generated_output(
        &self,
        output: shell_contract::ipc::SavedPublication,
        input: Option<shell_contract::ipc::NativeSavedInput>,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        accepted: shell_contract::hosts::PdfReloadAccepted,
        wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String> {
        docs_tabs::prepare_generated_output(
            output,
            input,
            participants,
            cancel,
            pending,
            additional,
            effects,
            accepted,
            wake,
        )
    }
    fn active_docs_view(&self) -> Option<ViewId> {
        docs_tabs::active(&self.cx)
    }
    fn set_menu_item_checked(&self, id: &str, checked: bool) {
        menu::set_menu_item_checked(id, checked);
    }
    fn set_menu_item_enabled(&self, id: &str, enabled: bool) {
        menu::set_menu_item_enabled(id, enabled);
    }
    fn rebuild_docs_menu(&self) {
        menu::rebuild_docs_menu();
    }
    fn prepare_docs_menu_refresh_owned(
        &self,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_d2::docs::hosts::OwnedMenuRefresh>, String> {
        menu::prepare_docs_menu_refresh_many_owned(
            &origin,
            &destination,
            &participants,
            &cancel,
            &pending,
            &effects,
        )
    }
    fn prepare_docs_menu_refresh_recovery(
        &self,
        receipt: shell_contract::ipc::RecoverySeedReceipt,
        receiver: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<shell_ipc_d2::docs::hosts::OwnedMenuRefresh>, String> {
        menu::prepare_docs_menu_refresh_recovery(receipt, receiver, cancel, pending, effects)
    }
    fn capture_rect(
        &self,
        view: ViewId,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> Result<Option<Vec<u8>>, String> {
        docs_image::capture(&self.cx.windows, view, x, y, width, height)
    }
    fn image_size(&self, bytes: &[u8]) -> Option<(f64, f64)> {
        docs_image::size(bytes)
    }
    fn write_clipboard_image(&self, bytes: &[u8], html: &str) {
        docs_image::write_clipboard(&self.cx.ui, bytes, html);
    }
    fn print_html_to_pdf(&self, html: String) -> Result<Vec<u8>, String> {
        docs_print::html_to_pdf(&self.cx, html)
    }
    fn print_html_to_pdf_owned(
        &self,
        html: String,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Vec<u8>, String> {
        docs_print::html_to_pdf_owned(&self.cx, html, origin, destination, cancel, pending)
    }
    fn net_fetch(
        &self,
        url: &str,
        permit: &main_core::docs::byte_handoff::MediaPermit,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn Response + Send>, String> {
        crate::host::net::ai::fetch::net_fetch_for_docs(
            url,
            crate::paths::resources_path(&self.cx.app).as_ref(),
            permit,
            cancel,
        )
    }
}
