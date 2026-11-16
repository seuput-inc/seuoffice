use super::*;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;
use objc2_app_kit::{NSWindow, NSWindowStyleMask};
use objc2_foundation::NSString;
use crate::native::WindowExt;

pub(super) fn is_native_fullscreen(window: &tauri::Window) -> Result<bool, String> {
    let handle = window.native_window_handle().map_err(|error| error.to_string())?;
    handle.with_window(|_| {
        let pointer = window.ns_window().map_err(|error| error.to_string())?;
        let native = unsafe { Retained::retain(pointer.cast::<NSWindow>()) }.ok_or("The original presenter NSWindow is unavailable")?;
        Ok(native.styleMask().contains(NSWindowStyleMask::FullScreen) || window.is_fullscreen().map_err(|error| error.to_string())?)
    }).map_err(|error| error.to_string())?
}

type Done = Box<dyn FnOnce(Result<(), String>) + Send>;

#[derive(Default)]
struct ExitState(AtomicU8);
impl ExitState {
    fn queue_exit(&self) -> bool {
        self.0.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }
    fn finish(&self) -> bool { self.0.swap(2, Ordering::AcqRel) != 2 }
    fn finished(&self) -> bool { self.0.load(Ordering::Acquire) == 2 }
}

struct Move {
    cx: HostContext,
    origin: RenderOwner,
    destination: RenderOwner,
    cancel: CancelToken,
    pending: CancelToken,
    native: tauri::Window,
    lifecycle: Arc<crate::views::NativeWindowLifecycle>,
    bounds: Display,
    observer: AtomicUsize,
    center: AtomicUsize,
    phase: ExitState,
    done: Mutex<Option<Done>>,
    losses: Mutex<Vec<shell_contract::runtime::CancelSubscription>>,
}

impl Move {
    fn remove_observer(&self) {
        debug_assert!(self.cx.ui.is_current());
        let observer = self.observer.swap(0, Ordering::AcqRel) as *mut AnyObject;
        if observer.is_null() { return; }
        let center = self.center.swap(0, Ordering::AcqRel) as *mut AnyObject;
        // These are owned Retained pointers transferred on this same UI thread.
        // Taking the observer once balances both transfers, including cancellation.
        unsafe {
            let observer = Retained::from_raw(observer).expect("the registered observer is retained");
            let center = Retained::from_raw(center).expect("the original notification center is retained");
            let _: () = msg_send![&*center, removeObserver: &*observer];
        }
    }

    fn complete(self: &Arc<Self>, answer: Result<(), String>) {
        if !self.phase.finish() { return; }
        let this = self.clone();
        self.cx.ui.post(Box::new(move || {
            this.remove_observer();
            let losses = std::mem::take(&mut *this.losses.lock().unwrap_or_else(|error| error.into_inner()));
            drop(losses);
            let done = this.done.lock().unwrap_or_else(|error| error.into_inner()).take();
            if let Some(done) = done { done(answer); }
        }));
    }

    fn observed_exit(self: &Arc<Self>) {
        if !self.phase.queue_exit() { return; }
        let permit = match self.lifecycle.admit_native_effect() {
            Ok(permit) => permit,
            Err(error) => return self.complete(Err(error)),
        };
        let this = self.clone();
        // Let every native notification recipient, including winit's delegate,
        // finish restoring fullscreen state before applying the original bounds.
        self.cx.ui.post(Box::new(move || {
            if !permit.is_current() {
                return this.complete(Err("The original presenter window is closing".into()));
            }
            this.remove_observer();
            if this.phase.finished() { return; }
            let answer = native_owner::run_accepted_ui_pair(this.cx.ui.as_ref(), &this.origin, &this.destination, &this.cancel, &this.pending, || {
                set_bounds(&this.native, this.bounds)
            }).and_then(|answer| answer).and_then(|()| {
                native_owner::run_accepted_ui_pair(this.cx.ui.as_ref(), &this.origin, &this.destination, &this.cancel, &this.pending, || {
                    this.native.set_fullscreen(true).map_err(|error| error.to_string())
                }).and_then(|answer| answer)
            });
            this.complete(answer);
        }));
    }

    fn register(self: &Arc<Self>) -> Result<(), String> {
        let handle = self.native.native_window_handle().map_err(|error| error.to_string())?;
        handle.with_window(|_| {
            let pointer = self.native.ns_window().map_err(|error| error.to_string())?;
            let native = unsafe { Retained::retain(pointer.cast::<NSWindow>()) }.ok_or("The original presenter NSWindow is unavailable")?;
            let object = Retained::as_ptr(&native);
            let class = AnyClass::get(c"NSNotificationCenter").ok_or("The native notification center class is unavailable")?;
            let center: *mut AnyObject = unsafe { msg_send![class, defaultCenter] };
            let center = unsafe { Retained::retain(center) }.ok_or("The native notification center is unavailable")?;
            let this = self.clone();
            let callback = block2::RcBlock::new(move |_: *mut AnyObject| {
                let _original_window = &native;
                this.observed_exit();
            });
            let name = NSString::from_str("NSWindowDidExitFullScreenNotification");
            let observer: *mut AnyObject = unsafe {
                msg_send![&*center, addObserverForName: &*name, object: object,
                    queue: std::ptr::null::<AnyObject>(), usingBlock: &*callback]
            };
            let observer = unsafe { Retained::retain(observer) }.ok_or("Registering the original fullscreen-exit observer failed")?;
            self.center.store(Retained::into_raw(center) as usize, Ordering::Release);
            self.observer.store(Retained::into_raw(observer) as usize, Ordering::Release);
            if self.phase.finished() { self.remove_observer(); }
            Ok(())
        }).map_err(|error| error.to_string())?
    }
}

pub(super) fn begin(
    cx: HostContext, origin: RenderOwner, destination: RenderOwner,
    cancel: CancelToken, pending: CancelToken, native: tauri::Window, lifecycle: Arc<crate::views::NativeWindowLifecycle>, target: Display, done: Done,
) {
    let bounds = match display_by_id(target.id) {
        Ok(native_display) => display(&native_display, true),
        Err(error) => return done(Err(error)),
    };
    let this = Arc::new(Move { cx, origin, destination, cancel, pending, native, lifecycle, bounds,
        observer: AtomicUsize::new(0), center: AtomicUsize::new(0), phase: ExitState::default(),
        done: Mutex::new(Some(done)), losses: Mutex::new(Vec::new()) });
    let Some(source_loss) = this.origin.loss_token() else { return this.complete(Err("The presenter source lease is unavailable".into())); };
    let Some(destination_loss) = this.destination.loss_token() else { return this.complete(Err("The audience destination lease is unavailable".into())); };
    for loss in [source_loss, destination_loss, this.cancel.clone(), this.pending.clone()] {
        let weak = Arc::downgrade(&this);
        let subscription = loss.on_cancel(move || {
            if let Some(this) = weak.upgrade() { this.complete(Err("The presenter move was cancelled".into())); }
        });
        this.losses.lock().unwrap_or_else(|error| error.into_inner()).push(subscription);
    }
    let weak = Arc::downgrade(&this);
    this.native.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            if let Some(this) = weak.upgrade() { this.complete(Err("The original presenter window was destroyed".into())); }
        }
    });
    let prepared = native_owner::run_accepted_ui_pair(this.cx.ui.as_ref(), &this.origin, &this.destination, &this.cancel, &this.pending, || {
        if this.phase.finished() { return Err("The presenter move was cancelled before observer registration".to_owned()); }
        this.register()
    }).and_then(|answer| answer);
    if let Err(error) = prepared { return this.complete(Err(error)); }
    let leaving = native_owner::run_accepted_ui_pair(this.cx.ui.as_ref(), &this.origin, &this.destination, &this.cancel, &this.pending, || {
        if this.phase.finished() { return Err("The presenter move was cancelled before fullscreen exit".to_owned()); }
        this.native.set_fullscreen(false).map_err(|error| error.to_string())
    }).and_then(|answer| answer);
    if let Err(error) = leaving { this.complete(Err(error)); }
}

#[cfg(test)]
mod tests {
    use super::ExitState;

    #[test]
    fn cancelled_move_cannot_queue_geometry_from_a_late_native_exit() {
        let state = ExitState::default();
        assert!(state.finish());
        assert!(!state.queue_exit());
        assert!(!state.finish());
    }

    #[test]
    fn cancellation_after_exit_but_before_ui_continuation_blocks_geometry() {
        let state = ExitState::default();
        assert!(state.queue_exit());
        assert!(!state.finished());
        assert!(state.finish());
        assert!(state.finished());
        assert!(!state.queue_exit());
    }

    #[test]
    fn duplicate_exit_and_completion_settle_once() {
        let state = ExitState::default();
        assert!(state.queue_exit());
        assert!(!state.queue_exit());
        assert!(state.finish());
        assert!(!state.finish());
    }
}
