//! Dispatch on the core thread: a decoded message to its handler, with the
//! surface check, cancellation per view, and the rule that every message is
//! answered exactly once, whatever the handler does (answers, drops its
//! responder, panics on the core thread or on a worker).

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use shell_contract::error::IpcError;
use shell_contract::ids::{RequestId, ViewId};
use shell_contract::ipc::{CallContext, ChannelKind, NativeContext, Reply, Responder, Sender};
use shell_contract::runtime::{CancelToken, RendererGeneration, Workers};
use shell_contract::value::IpcValue;

use crate::table::{Table, spec_of};

type Deliver = Box<dyn FnOnce(Reply) + Send>;

/// The one answer of one message
struct Once {
    channel: String,
    deliver: Mutex<Option<Deliver>>,
}

impl Once {
    fn answer(&self, reply: Reply) {
        let deliver = self.deliver.lock().unwrap().take();
        if let Some(deliver) = deliver {
            deliver(reply);
        }
    }
}

thread_local! {
    /// Answers whose responder was dropped while this thread unwound; the
    /// catch site that stops the unwind answers them with the panic's text
    static ORPHANS: RefCell<Vec<Arc<Once>>> = const { RefCell::new(Vec::new()) };
}

/// Dropped unanswered: outside a panic the handler forgot to answer (a bug),
/// inside one the catch site answers with the panic's text
struct Guard(Arc<Once>);

impl Drop for Guard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            ORPHANS.with(|o| o.borrow_mut().push(self.0.clone()));
        } else {
            let message = format!("the handler of '{}' dropped its reply", self.0.channel);
            self.0.answer(Err(error(message)));
        }
    }
}

fn error(message: String) -> IpcError {
    IpcError {
        name: Some("Error".into()),
        message,
    }
}

fn panic_text(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a handler panicked".into())
}

/// Runs `job`, and when it panics answers every reply it orphaned with the
/// panic's text (`name: "Error"`, as the contract's error section says)
pub fn catching(job: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(job)) {
        let text = panic_text(payload.as_ref());
        for once in ORPHANS.with(|o| std::mem::take(&mut *o.borrow_mut())) {
            once.answer(Err(error(text.clone())));
        }
    }
}

/// Workers whose jobs answer the replies they orphan by panicking
pub struct CatchingWorkers(pub Arc<dyn Workers>);

impl Workers for CatchingWorkers {
    fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
        self.0.spawn(Box::new(move || catching(job)));
    }
}

#[derive(Default)]
struct ViewCancellations {
    destroyed: CancelToken,
    renderers: Vec<(Option<RendererGeneration>, CancelToken)>,
}

/// The table and the per-view state of dispatch. Lives on the core thread.
pub struct Dispatcher {
    table: Table,
    cancels: RefCell<HashMap<ViewId, ViewCancellations>>,
}

impl Dispatcher {
    pub fn new(table: Table) -> Self {
        Dispatcher {
            table,
            cancels: RefCell::default(),
        }
    }

    pub fn table(&self) -> &Table {
        &self.table
    }

    /// One message of `sender`. `deliver` gets the answer once, from
    /// whichever thread the handler answers on: for `invoke` the shell puts
    /// it on the view's stream, for `send` it logs an error, for `sendSync`
    /// it unblocks the renderer.
    pub fn dispatch(
        &self,
        sender: Sender,
        kind: ChannelKind,
        request: Option<RequestId>,
        native: Option<NativeContext>,
        channel: &str,
        args: Vec<IpcValue>,
        deliver: Deliver,
    ) {
        let once = Arc::new(Once {
            channel: channel.into(),
            deliver: Mutex::new(Some(deliver)),
        });
        let Some(spec) = spec_of(channel) else {
            once.answer(Err(IpcError::no_handler(channel)));
            return;
        };
        if !spec.surfaces.contains(&sender.surface) {
            let message = format!("'{channel}' is not open to {:?}", sender.surface);
            once.answer(Err(error(message)));
            return;
        }
        let Some(handler) = self.table.handler(channel, kind) else {
            once.answer(Err(IpcError::no_handler(channel)));
            return;
        };
        let generation = native.as_ref().and_then(NativeContext::renderer).cloned();
        if generation.as_ref().is_some_and(|generation| !generation.is_active()) {
            once.answer(Err(error("the sender's renderer was replaced".into())));
            return;
        }
        let (cancel, view_loss) = {
            let mut cancels = self.cancels.borrow_mut();
            let entry = cancels.entry(sender.view).or_default();
            let cancel = if let Some((_, token)) = entry.renderers.iter().find(|(old, _)| match (old, &generation) {
                (Some(a), Some(b)) => a.same_generation(b),
                (None, None) => true,
                _ => false,
            }) {
                token.clone()
            } else {
                let token = CancelToken::default();
                entry.renderers.push((generation, token.clone()));
                token
            };
            (cancel, entry.destroyed.clone())
        };
        let native = native.map(|context| context.bind_dispatcher_cancellation(sender.view, cancel.clone(), view_loss));
        let cx = CallContext {
            sender,
            request,
            cancel,
            native,
        };
        let guard = Guard(once.clone());
        let reply = Responder::new(move |reply| guard.0.answer(reply));
        catching(|| handler.call(&cx, args, reply));
    }

    /// The view is gone: cancel every remaining document lifetime.
    pub fn view_destroyed(&self, view: ViewId) {
        let tokens = self.cancels.borrow_mut().remove(&view);
        if let Some(entry) = tokens {
            entry.destroyed.cancel();
            for (_, token) in entry.renderers { token.cancel(); }
        }
    }

    /// Cancel only calls owned by the renderer document that was replaced.
    pub fn renderer_replaced(&self, view: ViewId, old: &RendererGeneration) {
        let tokens = {
            let mut cancels = self.cancels.borrow_mut();
            let mut tokens = Vec::new();
            if let Some(entries) = cancels.get_mut(&view) {
                let mut index = 0;
                while index < entries.renderers.len() {
                    if entries.renderers[index].0.as_ref().is_some_and(|generation| generation.same_generation(old)) {
                        tokens.push(entries.renderers.remove(index).1);
                    } else {
                        index += 1;
                    }
                }
                // Keep the per-view destruction token even with no renderer
                // entries: an accepted transfer still observes physical close.
            }
            tokens
        };
        for token in tokens { token.cancel_renderer_retired(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::Table;
    use shell_contract::ids::WindowId;
    use shell_contract::ipc::{Handler, Registry};
    use shell_contract::{Batch, CHANNELS, Surface};
    use std::rc::Rc;
    use std::sync::mpsc;

    enum Does {
        Answer,
        Drop,
        Panic,
        PanicOnWorker,
        Keep(Rc<RefCell<Option<Responder>>>),
    }

    impl Handler for Does {
        fn call(&self, cx: &CallContext, _: Vec<IpcValue>, reply: Responder) {
            match self {
                Does::Answer => reply.reply(Ok(IpcValue::Bool(cx.cancel.is_cancelled()))),
                Does::Drop => drop(reply),
                Does::Panic => panic!("boom"),
                Does::PanicOnWorker => {
                    let workers = CatchingWorkers(Arc::new(crate::core_thread::ThreadWorkers));
                    workers.spawn(Box::new(move || {
                        let _keep = reply;
                        panic!("worker boom")
                    }));
                }
                Does::Keep(slot) => *slot.borrow_mut() = Some(reply),
            }
        }
    }

    fn w_invoke() -> (&'static str, Surface) {
        let s = CHANNELS
            .iter()
            .find(|s| s.batch == Batch::W && s.kinds == [ChannelKind::Invoke])
            .unwrap();
        (s.name, s.surfaces[0])
    }

    fn run(does: Does, surface: Option<Surface>) -> Reply {
        let (channel, open) = w_invoke();
        let mut table = Table::default();
        table
            .register(Batch::W, channel, ChannelKind::Invoke, Rc::new(does))
            .unwrap();
        let d = Dispatcher::new(table);
        let (tx, rx) = mpsc::channel();
        let sender = Sender {
            view: ViewId(1),
            window: WindowId(1),
            surface: surface.unwrap_or(open),
        };
        d.dispatch(
            sender,
            ChannelKind::Invoke,
            Some(RequestId(7)),
            None,
            channel,
            vec![],
            Box::new(move |r| tx.send(r).unwrap()),
        );
        let reply = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(rx.try_recv().is_err(), "answered twice");
        reply
    }

    fn message(reply: Reply) -> String {
        reply.unwrap_err().to_string()
    }

    #[test]
    fn every_message_is_answered_once() {
        assert_eq!(run(Does::Answer, None), Ok(IpcValue::Bool(false)));
        assert!(message(run(Does::Drop, None)).contains("dropped its reply"));
        assert_eq!(message(run(Does::Panic, None)), "Error: boom");
        assert_eq!(
            message(run(Does::PanicOnWorker, None)),
            "Error: worker boom"
        );
    }

    #[test]
    fn refuses_a_surface_the_spec_does_not_list() {
        let (_, open) = w_invoke();
        let other = [Surface::ShellUpdate, Surface::Shell]
            .into_iter()
            .find(|s| *s != open)
            .unwrap();
        assert!(message(run(Does::Answer, Some(other))).contains("is not open to"));
    }

    #[test]
    fn unknown_and_unregistered_channels_answer_as_electron() {
        let d = Dispatcher::new(Table::default());
        let (channel, surface) = w_invoke();
        for name in ["no:such", channel] {
            let (tx, rx) = mpsc::channel();
            let sender = Sender {
                view: ViewId(1),
                window: WindowId(1),
                surface,
            };
            d.dispatch(
                sender,
                ChannelKind::Invoke,
                None,
                None,
                name,
                vec![],
                Box::new(move |r| tx.send(r).unwrap()),
            );
            assert_eq!(
                message(rx.recv().unwrap()),
                format!("Error: No handler registered for '{name}'")
            );
        }
    }

    #[test]
    fn destroying_a_view_cancels_its_pending_calls() {
        let (channel, surface) = w_invoke();
        let slot = Rc::new(RefCell::new(None));
        let mut table = Table::default();
        table
            .register(
                Batch::W,
                channel,
                ChannelKind::Invoke,
                Rc::new(Does::Keep(slot.clone())),
            )
            .unwrap();
        let d = Dispatcher::new(table);
        let sender = Sender {
            view: ViewId(3),
            window: WindowId(1),
            surface,
        };
        let (tx, rx) = mpsc::channel();
        d.dispatch(
            sender,
            ChannelKind::Invoke,
            None,
            None,
            channel,
            vec![],
            Box::new(move |r| tx.send(r).unwrap()),
        );
        assert!(rx.try_recv().is_err());
        d.view_destroyed(ViewId(3));
        // the handler sees the cancel; its late answer still arrives once
        // (the shell's stream drops it for a gone view)
        let reply = slot.borrow_mut().take().unwrap();
        reply.reply(Ok(IpcValue::Null));
        assert_eq!(rx.recv().unwrap(), Ok(IpcValue::Null));
    }

    #[test]
    fn dispatch_retains_the_captured_native_request_context() {
        struct Check(Arc<std::sync::atomic::AtomicUsize>);
        impl Handler for Check {
            fn call(&self, cx: &CallContext, _: Vec<IpcValue>, reply: Responder) {
                let same = cx.native.as_ref()
                    .and_then(|native| native.downcast_ref::<Arc<std::sync::atomic::AtomicUsize>>())
                    .is_some_and(|captured| Arc::ptr_eq(captured, &self.0));
                reply.reply(Ok(IpcValue::Bool(same)));
            }
        }
        let (channel, surface) = w_invoke();
        let captured = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut table = Table::default();
        table.register(Batch::W, channel, ChannelKind::Invoke, Rc::new(Check(captured.clone()))).unwrap();
        let dispatcher = Dispatcher::new(table);
        let (tx, rx) = mpsc::channel();
        dispatcher.dispatch(
            Sender { view: ViewId(1), window: WindowId(1), surface },
            ChannelKind::Invoke,
            None,
            Some(NativeContext::new(captured)),
            channel,
            vec![],
            Box::new(move |answer| tx.send(answer).unwrap()),
        );
        assert_eq!(rx.recv().unwrap(), Ok(IpcValue::Bool(true)));
    }

    #[test]
    fn replacing_an_old_renderer_does_not_cancel_its_successor() {
        struct Capture(Rc<RefCell<Vec<CancelToken>>>);
        impl Handler for Capture {
            fn call(&self, cx: &CallContext, _: Vec<IpcValue>, reply: Responder) {
                self.0.borrow_mut().push(cx.cancel.clone());
                reply.reply(Ok(IpcValue::Null));
            }
        }
        let (channel, surface) = w_invoke();
        let captured = Rc::new(RefCell::new(Vec::new()));
        let mut table = Table::default();
        table.register(Batch::W, channel, ChannelKind::Invoke, Rc::new(Capture(captured.clone()))).unwrap();
        let dispatcher = Dispatcher::new(table);
        let sender = Sender { view: ViewId(1), window: WindowId(1), surface };
        let old = RendererGeneration::new();
        let new = RendererGeneration::new();
        for generation in [&old, &new] {
            dispatcher.dispatch(sender, ChannelKind::Invoke, None,
                Some(NativeContext::with_renderer((), generation.clone())), channel, vec![], Box::new(|_| {}));
        }
        old.revoke();
        dispatcher.renderer_replaced(sender.view, &old);
        let captured = captured.borrow();
        assert!(captured[0].is_cancelled());
        assert!(!captured[1].is_cancelled());
    }

    #[test]
    fn revoked_renderer_cannot_dispatch_queued_request() {
        let (channel, surface) = w_invoke();
        let mut table = Table::default();
        table.register(Batch::W, channel, ChannelKind::Invoke, Rc::new(Does::Answer)).unwrap();
        let dispatcher = Dispatcher::new(table);
        let generation = RendererGeneration::new();
        let native = NativeContext::with_renderer((), generation.clone());
        generation.revoke();
        let (tx, rx) = mpsc::channel();
        dispatcher.dispatch(
            Sender { view: ViewId(1), window: WindowId(1), surface },
            ChannelKind::Invoke, None, Some(native), channel, vec![],
            Box::new(move |reply| tx.send(reply).unwrap()),
        );
        assert!(rx.recv().unwrap().is_err());
    }

    #[test]
    fn dispatcher_provenance_shares_epoch_cancel_but_preserves_view_loss_after_retirement() {
        struct Capture(Rc<RefCell<Vec<CallContext>>>);
        impl Handler for Capture {
            fn call(&self, cx: &CallContext, _: Vec<IpcValue>, reply: Responder) {
                self.0.borrow_mut().push(cx.clone());
                reply.reply(Ok(IpcValue::Null));
            }
        }
        let (channel, surface) = w_invoke();
        let captured = Rc::new(RefCell::new(Vec::new()));
        let mut table = Table::default();
        table.register(Batch::W, channel, ChannelKind::Invoke, Rc::new(Capture(captured.clone()))).unwrap();
        let dispatcher = Dispatcher::new(table);
        let sender = Sender { view: ViewId(1), window: WindowId(1), surface };
        let old = RendererGeneration::new();
        for _ in 0..2 {
            dispatcher.dispatch(sender, ChannelKind::Invoke, None,
                Some(NativeContext::with_renderer((), old.clone())), channel, vec![], Box::new(|_| {}));
        }
        let contexts = captured.borrow().clone();
        assert!(contexts[0].cancel.same_token(&contexts[1].cancel));
        let first = shell_contract::ipc::NativeRequestOwner { view: sender.view, context: contexts[0].native.clone().unwrap() };
        let provenance = first.dispatcher_cancellation_for(&contexts[0].cancel).unwrap();
        let view_loss = provenance.view_loss();
        let external = provenance.external_loss();
        assert!(first.dispatcher_cancellation_for(&CancelToken::default()).is_none());
        old.revoke();
        dispatcher.renderer_replaced(sender.view, &old);
        assert!(contexts[0].cancel.is_cancelled());
        assert!(!external.is_cancelled());
        assert!(!view_loss.is_cancelled());
        assert!(dispatcher.cancels.borrow().get(&sender.view).unwrap().renderers.is_empty());
        let successor = RendererGeneration::new();
        dispatcher.dispatch(sender, ChannelKind::Invoke, None,
            Some(NativeContext::with_renderer((), successor)), channel, vec![], Box::new(|_| {}));
        let next = captured.borrow()[2].clone();
        assert!(!next.cancel.same_token(&contexts[0].cancel));
        let next_owner = shell_contract::ipc::NativeRequestOwner { view: sender.view, context: next.native.unwrap() };
        assert!(next_owner.dispatcher_cancellation_for(&next.cancel).unwrap().view_loss().same_token(&view_loss));
        dispatcher.view_destroyed(sender.view);
        assert!(view_loss.is_cancelled());
        assert!(next.cancel.is_cancelled());
        assert!(dispatcher.cancels.borrow().get(&sender.view).is_none());
    }
}
