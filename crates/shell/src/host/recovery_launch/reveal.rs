use std::sync::{Arc, Mutex};

use shell_contract::{WindowId, ipc::{NativeInvocationOperation, NativeRequestOwner}, runtime::{Services, Views}};
use tauri::Manager;
use crate::native::{BrowserState, WebviewExt};

use crate::host::HostContext;
use crate::ipc::native_owner::NativeOwnerAuthority;

#[derive(Clone)]
pub(super) struct CapturedReveal {
    window: tauri::Window,
    webview: tauri::Webview,
    browser: BrowserState,
    owner: NativeRequestOwner,
}

pub(super) fn capture(cx: &HostContext, shell_window: WindowId, owner: &NativeRequestOwner) -> Result<CapturedReveal, String> {
    let sender = cx.views.sender_of(owner.view).ok_or("The selected shell view is unavailable")?;
    if sender.window != shell_window || sender.surface != shell_contract::Surface::Shell {
        return Err("The selected shell view does not belong to the selected window".into());
    }
    let generation = owner.context.renderer().ok_or("The selected shell renderer is unavailable")?;
    let current = cx.views.renderer_generation(owner.view).ok_or("The selected shell renderer is unavailable")?;
    let proof = owner.context.authority_ref::<NativeOwnerAuthority>().ok_or("The selected shell native owner is unavailable")?;
    if !current.same_generation(generation) || !generation.is_active() {
        return Err("The selected shell owner was replaced".into());
    }
    let browser = proof.origin.browser().clone();
    let label = cx.views.label_of_window(shell_window).ok_or("The selected shell window is unavailable")?;
    let window = cx.app.get_window(&label).ok_or("The selected shell window is unavailable")?;
    let webview = cx.windows.webview(owner.view).ok_or("The selected shell view is unavailable")?;
    if webview.window().label() != window.label() { return Err("The selected shell view moved to another window".into()); }
    Ok(CapturedReveal { window, webview, owner: owner.clone(), browser })
}

impl CapturedReveal {
    pub(super) fn run(&self, services: &Services, operation: NativeInvocationOperation, owner: NativeRequestOwner,
        done: Box<dyn FnOnce(Result<(), String>)>) {
        let matches = self.owner.view == owner.view
            && self.owner.context.renderer().zip(owner.context.renderer()).is_some_and(|(a, b)| a.same_generation(b))
            && self.owner.context.authority_ref::<NativeOwnerAuthority>().zip(owner.context.authority_ref::<NativeOwnerAuthority>())
                .is_some_and(|(a, b)| a.origin == b.origin);
        if !matches { return done(Err("The selected startup shell owner was replaced".into())); }
        let reply = Arc::new(Mutex::new(Some(shell_ipc_w::home_files::back_to_core(services.core.clone(), done))));
        let callback = reply.clone();
        let window = self.window.clone();
        let webview = self.webview.clone();
        let captured_owner = self.owner.clone();
        let captured_browser = self.browser.clone();
        let ui = services.ui.clone();
        let result = self.webview.with_native_async(move |native| {
            let browser = match native {
                Ok(native) => native.identity.clone(),
                Err(error) => {
                    if let Some(done) = callback.lock().unwrap_or_else(|error| error.into_inner()).take() { done(Err(error.to_string())); }
                    return;
                }
            };
            let current_ui = ui.clone();
            ui.post(Box::new(move || {
                let result = if !browser.is_same_browser(&captured_browser)
                    || !captured_owner.context.authority_ref::<NativeOwnerAuthority>()
                        .is_some_and(|proof| captured_browser.is_same_browser(proof.origin.browser())) {
                    Err("The selected shell browser was replaced".into())
                } else if captured_browser.with_live_browser(|| ()).is_none() {
                    Err("The selected shell browser is closed".into())
                } else {
                    crate::ipc::native_owner::run_accepted_native_operation_ui_many(
                        current_ui.as_ref(), &operation, &captured_owner, &[owner], operation.cancel(), operation.shutdown(), || {
                            if webview.window().label() != window.label() {
                                return Err("The selected shell view moved to another window".into());
                            }
                            if window.is_minimized().map_err(|error| error.to_string())? {
                                window.unminimize().map_err(|error| error.to_string())?;
                            }
                            window.show().map_err(|error| error.to_string())?;
                            window.set_focus().map_err(|error| error.to_string())
                        },
                    ).and_then(|result| result)
                };
                if let Some(done) = callback.lock().unwrap_or_else(|error| error.into_inner()).take() { done(result); }
            }));
        });
        if let Err(error) = result {
            if let Some(done) = reply.lock().unwrap_or_else(|error| error.into_inner()).take() { done(Err(error.to_string())); }
        }
    }
}
