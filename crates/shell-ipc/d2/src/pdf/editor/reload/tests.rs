use super::super::{
    editors::PdfEditors,
    fakes,
    hosts::{PdfEngineHost, PdfShellHost},
};
use crate::testing::Rt;
use shell_contract::{
    Surface,
    editors::DocumentEditors,
    hosts::OwnedEffectsHost,
    ipc::{NativeContext, NativeRequestOwner},
    runtime::{CancelToken, RendererGeneration},
};
use std::sync::{Arc, Mutex};

#[test]
fn reload_actor_preserves_old_slot_until_reserved_document_completes() {
    for matched in [false, true] {
        let mut rt = Rt::new(|hosts| {
            hosts.insert::<PdfEngineHost>(fakes::engine(&serde_json::Value::Null));
            hosts.insert::<PdfShellHost>(fakes::shell(&serde_json::Value::Null));
            hosts.insert(Arc::new(Mutex::new(
                main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new(
                    "test",
                ),
            )));
            crate::pdf::tools::fakes(hosts, &serde_json::Value::Null);
        });
        rt.register(crate::pdf::entries).unwrap();
        let sender = rt.view(1, Surface::Pdf);
        let path = rt.dir.join("generated.pdf");
        std::fs::write(&path, b"%PDF generated").unwrap();
        let editor = rt.provided().all::<PdfEditors>().pop().unwrap();
        editor.open_view(sender.view, path.to_str());
        editor.0.with_view(sender.view, |slot| slot.dirty = true);
        let renderer = rt.services.views.renderer_generation(sender.view).unwrap();
        let native = rt
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .unwrap()
            .capture_owner(sender.view, &renderer)
            .unwrap();
        let existing = editor.capture_document_owner(sender.view, native).unwrap();
        let old_slot = existing.pdf_document().unwrap().clone();
        let old_revision = existing.pdf_revision().unwrap().clone();
        assert!(
            editor
                .prepare_generated_reload(&existing, renderer)
                .is_err()
        );
        let reserved = RendererGeneration::new();
        let prepared = editor
            .prepare_generated_reload(&existing, reserved.clone())
            .unwrap();
        assert!(prepared.matches_actor());
        assert!(old_slot.is_live());
        assert!(old_revision.is_live());
        assert!(editor.is_dirty(sender.view));
        assert!(
            editor
                .prepare_generated_reload(&existing, RendererGeneration::new())
                .is_err()
        );
        let actual = if matched {
            reserved.clone()
        } else {
            RendererGeneration::new()
        };
        let successor = NativeRequestOwner {
            view: sender.view,
            context: NativeContext::with_owner_loss((), actual, CancelToken::default())
                .with_document_lease(RendererGeneration::new()),
        };
        let original_renderer = existing.context.renderer().unwrap().clone();
        let same_context = Arc::new(move |owner: &NativeRequestOwner| {
            owner
                .context
                .renderer()
                .is_some_and(|renderer| renderer.same_generation(&original_renderer))
        });
        let lineage = shell_contract::hosts::AcceptedPdfReload::from_native_admission(
            existing.clone(),
            reserved.clone(),
            CancelToken::default(),
            reserved.loss_token(),
            same_context,
        );
        let result = prepared.complete(successor, lineage);
        if matched {
            let receipt = result.unwrap();
            assert!(
                receipt
                    .owner()
                    .context
                    .renderer()
                    .unwrap()
                    .same_generation(&reserved)
            );
            assert!(
                !receipt
                    .owner()
                    .pdf_document()
                    .unwrap()
                    .same_document(&old_slot)
            );
            assert!(
                !receipt
                    .owner()
                    .pdf_revision()
                    .unwrap()
                    .same_document(&old_revision)
            );
            assert!(!old_slot.is_live());
            assert!(!old_revision.is_live());
            assert!(!editor.is_dirty(sender.view));
        } else {
            assert!(result.is_err());
            assert!(old_slot.is_live());
            assert!(old_revision.is_live());
            assert!(editor.is_dirty(sender.view));
            assert!(
                editor
                    .prepare_generated_reload(&existing, RendererGeneration::new())
                    .is_ok()
            );
        }
    }
}
