//! Native confirmation for a hardware token's protected authentication path.
use std::sync::Arc;
use shell_contract::{hosts::MessageBox, ipc::NativeRequestOwner, runtime::CancelToken};
use crate::host::{HostContext, message_box::cancellable::{CancellableDialogs, DialogAuthority}};
use super::crypto::{ProtectedTokenInteraction, SigningAuthority};

struct OwnerAuthority(Arc<dyn SigningAuthority>);
impl DialogAuthority for OwnerAuthority {
    fn owner_loss(&self, owner: &NativeRequestOwner) -> Option<CancelToken> { self.0.owner_loss(owner) }
    fn is_current(&self, owner: &NativeRequestOwner, cancel: &CancelToken) -> bool { self.0.is_current(owner, cancel) }
    fn with_admission(&self, owner: &NativeRequestOwner, cancel: &CancelToken, action: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        self.0.with_admission(owner, cancel, action)
    }
}

pub(crate) struct NativeTokenDialog {
    authority: Arc<dyn SigningAuthority>,
    dialogs: CancellableDialogs,
}

impl NativeTokenDialog {
    pub(crate) fn new(cx: HostContext, authority: Arc<dyn SigningAuthority>) -> Self {
        let dialogs = CancellableDialogs::new(cx, Arc::new(OwnerAuthority(authority.clone())));
        Self { authority, dialogs }
    }
}

fn plain_token_name(name: &str) -> String {
    name.chars().map(|c| if c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') { '\u{fffd}' } else { c }).collect()
}

impl ProtectedTokenInteraction for NativeTokenDialog {
    fn confirm(&self, owner: &NativeRequestOwner, request_cancel: &CancelToken, entry_cancel: &CancelToken, token_name: &str) -> Result<bool, String> {
        if entry_cancel.is_cancelled() || !self.authority.is_current(owner, request_cancel) {
            return Err("certificate dialog owner expired".into());
        }
        let name = plain_token_name(token_name);
        let options = MessageBox {
            kind: Some("info".into()),
            title: Some("Security token".into()),
            message: "Complete authentication on your security token.".into(),
            detail: Some(format!("Token: {name}\n\nAfter authenticating on the token, select Continue. Do not enter your PIN in this app.")),
            buttons: vec!["Continue".into(), "Cancel".into()],
            default_id: Some(1), cancel_id: Some(1), checkbox_label: None,
        };
        let answer = self.dialogs.show_cancellable(owner, &options, request_cancel, entry_cancel)?;
        if entry_cancel.is_cancelled() || !self.authority.is_current(owner, request_cancel) {
            return Err("certificate dialog owner expired".into());
        }
        Ok(answer.response == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::plain_token_name;

    #[test]
    fn token_labels_remain_plain_and_cannot_change_prompt_direction() {
        assert_eq!(plain_token_name("Card <b>One</b>"), "Card <b>One</b>");
        assert_eq!(plain_token_name("Card\0\u{202e}evil\nnext"), "Card\u{fffd}\u{fffd}evil\u{fffd}next");
    }
}
