use super::*;
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{CallContext, NativeInvocationDocument, NativeInvocationSelection, NativeInvocationTarget, NativeRequestOwner};
use shell_contract::runtime::CancelToken;

#[derive(Clone)]
pub(crate) struct HeadlessPublication {
    pub document: NativeInvocationDocument,
    pub owner: NativeRequestOwner,
    pub cancel: CancelToken,
    pub pending: CancelToken,
    pub effects: OwnedEffectsHost,
}

impl HeadlessPublication {
    pub(crate) fn admission(&self) -> main_core::packages::electron_utils::atomic_write::owned::Admission {
        let publication = self.clone();
        Arc::new(move |effect| {
            let mut effect = Some(effect);
            let mut result = None;
            publication.effects.with_native_invocation_effect(
                &publication.document, &publication.owner, &[], &publication.cancel, &publication.pending,
                &mut || {
                    result = Some(effect.take().ok_or("the text export file effect was admitted twice")?());
                    Ok(())
                },
            ).map_err(|message| main_core::node_fs::NodeError { code: None, message })?;
            result.ok_or_else(|| main_core::node_fs::NodeError { code: None, message: "the text export file effect was not admitted".into() })
        })
    }
}

impl TextDocs {
    pub(crate) fn headless_export_invocation(
        self: &Rc<Self>, view: ViewId, input: &str, target: IpcValue, timeout: Duration,
        invocation: NativeInvocationTarget, then: HeadlessSettle,
    ) {
        if invocation.view() != view {
            return then(Err("the text export target does not match its reservation".into()));
        }
        self.headless_authority.borrow_mut().insert(view, (invocation, CancelToken::default()));
        self.headless_export(view, input, target, timeout, then);
    }

    fn headless_owner(&self, cx: &CallContext) -> Result<Option<HeadlessPublication>, IpcError> {
        let target = self.headless_authority.borrow().get(&cx.sender.view).cloned();
        let Some((target, pending)) = target else {
            if !self.is_headless() { return Ok(None); }
            #[cfg(any(test, feature = "probe"))]
            if self.services.hosts.get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>().is_some() {
                return Ok(None);
            }
            return Err(kit::error("the headless text export is no longer pending"));
        };
        let context = cx.native.clone().ok_or_else(|| kit::error("the text export has no authenticated native sender"))?;
        let owner = self.capture_document_owner(cx.sender.view, NativeRequestOwner { view: cx.sender.view, context })?;
        let document = target.registered_document().map_err(kit::error)?;
        let effects = self.services.hosts.get::<OwnedEffectsHost>().cloned().ok_or_else(|| kit::error("text export admission is unavailable"))?;
        Ok(Some(HeadlessPublication { document, owner, cancel: cx.cancel.clone(), pending, effects }))
    }

    pub(crate) fn capture_headless_publication(&self, cx: &CallContext, path: &str, format: &str) -> Result<Option<HeadlessPublication>, IpcError> {
        let Some(publication) = self.headless_owner(cx)? else { return Ok(None); };
        match publication.document.operation().selection() {
            NativeInvocationSelection::HeadlessExport { output, format: selected, .. }
                if output == Path::new(path) && selected == format => {}
            _ => return Err(kit::error("the text export does not match the selected output")),
        }
        publication.effects.with_native_invocation_effect(&publication.document, &publication.owner, &[], &publication.cancel, &publication.pending, &mut || Ok(())).map_err(kit::error)?;
        Ok(Some(publication))
    }

    pub(super) fn admit_headless_message(&self, cx: &CallContext, effect: impl FnOnce() -> Result<(), String>) -> Result<(), IpcError> {
        let Some(publication) = self.headless_owner(cx)? else { return effect().map_err(kit::error); };
        let mut effect = Some(effect);
        publication.effects.with_native_invocation_effect(&publication.document, &publication.owner, &[], &publication.cancel, &publication.pending, &mut || {
            effect.take().ok_or("the text export message was admitted twice")?()
        }).map_err(kit::error)
    }
}
