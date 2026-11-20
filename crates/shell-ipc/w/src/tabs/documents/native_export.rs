use super::TabsState;
use shell_contract::hosts::{
    CommittedGeneratedPath, GeneratedPathOutcome, GeneratedPathWake, OwnedEffectsHost,
};
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::CancelToken;

impl TabsState {
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_generated_output(
        &self,
        output: shell_contract::ipc::SavedPublication,
        input: Option<shell_contract::ipc::NativeSavedInput>,
        kind: &'static str,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: OwnedEffectsHost,
        accepted: shell_contract::hosts::PdfReloadAccepted,
        wake: GeneratedPathWake,
    ) -> Result<crate::tabs::PreparedGeneratedOperation, String> {
        if !matches!(kind, "pdf" | "docx" | "odt" | "generated" | "reveal") {
            return Err(format!("Unsupported generated export kind: {kind}"));
        }
        if input
            .as_ref()
            .is_some_and(|input| !input.output().same_publication(&output))
        {
            return Err("The saved input does not match the committed export".into());
        }
        let state = self.this.clone();
        let reveal_output = output.clone();
        let reveal_participants = participants.clone();
        let reveal_cancel = cancel.clone();
        let reveal_pending = pending.clone();
        let mut reveal_additional = additional.clone();
        let reveal_effects = effects.clone();
        let reveal = Box::new(move |operation_cancel: CancelToken,
                                    finish: Box<
            dyn FnOnce(Result<GeneratedPathOutcome, String>) + Send,
        >| {
            let Some(state) = state.upgrade() else {
                return finish(Err("The tab manager is unavailable".into()));
            };
            reveal_additional.push(operation_cancel);
            state.reveal_publication(
                reveal_output,
                reveal_participants,
                reveal_cancel,
                reveal_pending,
                reveal_additional,
                reveal_effects,
                finish,
            );
        });
        self.prepare_committed_output_owned(
            output,
            input,
            participants,
            cancel,
            pending,
            additional,
            effects,
            accepted,
            Some(reveal),
            kind == "reveal",
            wake,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn reveal_publication(
        &self,
        output: shell_contract::ipc::SavedPublication,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: OwnedEffectsHost,
        done: Box<dyn FnOnce(Result<GeneratedPathOutcome, String>) + Send>,
    ) {
        let extra: Vec<_> = additional.iter().collect();
        if let Err(error) = output.with_effect(
            effects.as_ref(),
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || Ok(()),
        ) {
            return done(Err(error));
        }
        let hook_output = output.clone();
        let hook_participants = participants.clone();
        let hook_cancel = cancel.clone();
        let hook_pending = pending.clone();
        let hook_additional = additional.clone();
        self.hooks.reveal_written_publication(
            hook_output,
            hook_participants,
            hook_cancel,
            hook_pending,
            hook_additional,
            Box::new(move |result| {
                let receipt = result.and_then(|()| match output {
                    shell_contract::ipc::SavedPublication::Native(native) => {
                        let selection = native.selection().clone();
                        let target = selection.binding().owner().clone();
                        CommittedGeneratedPath::from_existing_document_save(
                            selection,
                            target,
                            participants,
                            cancel,
                            pending,
                            additional,
                            effects,
                        )
                    }
                    shell_contract::ipc::SavedPublication::Renderer(renderer) => {
                        let mut captured = participants;
                        captured.extend_from_slice(renderer.participants());
                        let mut controls = additional;
                        controls.push(renderer.cancel().clone());
                        controls.push(renderer.pending().clone());
                        controls.extend_from_slice(renderer.additional());
                        CommittedGeneratedPath::from_loaded_target(
                            renderer.origin().clone(),
                            renderer.destination().clone(),
                            renderer.destination().clone(),
                            cancel,
                            pending,
                            effects,
                        )
                        .and_then(|receipt| receipt.with_captured_participants(&captured))
                        .and_then(|receipt| receipt.with_captured_cancellations(&controls))
                    }
                });
                done(
                    receipt
                        .map(|receipt| GeneratedPathOutcome::Opened(std::sync::Arc::new(receipt))),
                );
            }),
        );
    }
}
