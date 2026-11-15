use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{CallContext, NativeRequestOwner};
use shell_contract::runtime::{CancelToken, RendererGeneration};
use shell_contract::IpcError;
use crate::value::thrown;

#[derive(Clone)]
pub(crate) struct SeedAuthority {
    pub source: NativeRequestOwner,
    pub cancel: CancelToken,
    pub created: RendererGeneration,
}

impl SeedAuthority {
    pub fn stage<T>(&self, effects: &OwnedEffectsHost, participants: &[NativeRequestOwner], pending: &CancelToken, commit: impl FnOnce() -> T) -> Result<T, String> {
        let mut commit = Some(commit); let mut value = None;
        effects.with_created_many_effect(&self.source, participants, &self.created, &self.cancel, pending, &mut || {
            let commit = commit.take().ok_or_else(|| "The seed was already staged".to_owned())?;
            value = Some(commit()); Ok(())
        })?;
        value.ok_or_else(|| "The seed was not staged".to_owned())
    }

    pub fn consume<T>(&self, effects: &OwnedEffectsHost, cx: &CallContext, commit: impl FnOnce() -> T) -> Result<T, IpcError> {
        let context = cx.native.clone().ok_or_else(|| thrown("The seed consumer has no native document"))?;
        if !context.renderer().is_some_and(|renderer| renderer.same_generation(&self.created)) {
            return Err(thrown("The seed belongs to another editor document"));
        }
        let target = NativeRequestOwner { view: cx.sender.view, context };
        let mut commit = Some(commit); let mut value = None;
        effects.with_owned_effect(&target, &target, &cx.cancel, &cx.cancel, &mut || {
            if cx.cancel.is_cancelled() { return Err("The seed consumer was cancelled".into()); }
            let commit = commit.take().ok_or_else(|| "The seed was already consumed".to_owned())?;
            value = Some(commit()); Ok(())
        }).map_err(thrown)?;
        value.ok_or_else(|| thrown("The seed was not consumed"))
    }
}
