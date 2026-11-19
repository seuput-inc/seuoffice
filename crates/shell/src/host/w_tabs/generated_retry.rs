use std::{rc::Rc, sync::Arc};

use shell_contract::{
    hosts::{MessageBox, OwnedEffectsHost},
    ipc::{NativeRequestOwner, SavedPublication},
    runtime::{CancelToken, Thread},
};
use shell_ipc_w::tabs::{
    ShellWindow,
    host::{CapturedGeneratedRetryPrompt, GeneratedRetryChoice},
};

use super::Hooks;

struct Prompt {
    ui: Arc<dyn Thread>,
    core: Arc<dyn Thread>,
    effects: OwnedEffectsHost,
    lifecycle: Arc<crate::views::NativeWindowLifecycle>,
    output: SavedPublication,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    korean: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn capture(
    hooks: &Hooks,
    shell: ShellWindow,
    owner: NativeRequestOwner,
    output: SavedPublication,
    mut participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
) -> Result<Rc<dyn CapturedGeneratedRetryPrompt>, String> {
    if owner.view != shell.view {
        return Err("The retry prompt Shell owner does not match its window".into());
    }
    let generation = owner
        .context
        .renderer()
        .ok_or("The retry prompt Shell renderer is unavailable")?;
    let effects = hooks
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .cloned()
        .ok_or("The retry prompt authority is unavailable")?;
    participants.push(owner.clone());
    let extra: Vec<_> = additional.iter().collect();
    let mut captured = None;
    output.with_effect(
        &*effects,
        &participants,
        &cancel,
        &pending,
        &extra,
        &mut || {
            let (window, lifecycle) = hooks
                .cx
                .views
                .view_window_captured(shell.view, generation)?;
            if window != shell.window {
                return Err("The retry prompt Shell window changed".into());
            }
            captured = Some(lifecycle);
            Ok(())
        },
    )?;
    use shell_ipc_w::editor_files::UiLanguage;
    let korean = crate::host::w_files::ShellLanguage(hooks.cx.ui.clone())
        .lang()
        .as_str()
        == "ko";
    Ok(Rc::new(Prompt {
        ui: hooks.cx.ui.clone(),
        core: hooks.services.core.clone(),
        effects,
        lifecycle: captured.ok_or("The retry prompt Shell was not captured")?,
        output,
        participants,
        cancel,
        pending,
        additional,
        korean,
    }))
}

impl CapturedGeneratedRetryPrompt for Prompt {
    fn show(&self, error: String, done: Box<dyn FnOnce(Result<GeneratedRetryChoice, String>)>) {
        let parent = match crate::host::dialog::capture_owned_lifecycle(self.lifecycle.clone()) {
            Ok(parent) => Some(parent),
            Err(error) => return done(Err(error)),
        };
        let (message, retry, cancel) = if self.korean {
            ("파일 정리를 다시 시도하시겠습니까?", "다시 시도", "취소")
        } else {
            ("Retry cleaning up the file?", "Retry", "Cancel")
        };
        let options = MessageBox {
            kind: Some("warning".into()),
            message: message.into(),
            detail: Some(error),
            buttons: vec![retry.into(), cancel.into()],
            default_id: Some(1),
            cancel_id: Some(1),
            ..Default::default()
        };
        let done = shell_ipc_w::home_files::back_to_core(
            self.core.clone(),
            move |result: Result<shell_contract::hosts::MessageBoxAnswer, String>| {
                done(result.and_then(|answer| match answer.response {
                    0 => Ok(GeneratedRetryChoice::Retry),
                    1 => Ok(GeneratedRetryChoice::Cancel),
                    _ => Err("The retry dialog returned an unknown choice".into()),
                }));
            },
        );
        crate::host::dialog::owned::run_saved_publication(
            self.ui.clone(),
            self.effects.clone(),
            self.output.clone(),
            self.participants.clone(),
            self.cancel.clone(),
            self.pending.clone(),
            self.additional.clone(),
            move |session| {
                crate::host::dialog::owned::message(
                    &options,
                    crate::host::dialog::checked_owned_parent(&parent, &session)?,
                    &session,
                )
            },
            done,
        );
    }
}
