//! The helpers every slides handler shares: the lookup of the sender's
//! session, the journaled transaction pipeline of `slides-main.ts`
//! (`journaledTxn`, `sessionTxn`), history and deck broadcast scheduling with
//! its `setImmediate` flush, `rebuildSlide`, `buildSlideUpdate`, and the
//! small object and number helpers the TypeScript shims lean on.
//!
//! A handler of an edit channel is, in this crate:
//!
//! ```ignore
//! fn edit_x(app: &App, cx: &CallContext, args: Args) -> Reply {
//!     let op = args.get(0);
//!     app.with_session(cx.sender.view, |app, s| {
//!         let slide_index = prop(op, "slideIndex")?;
//!         let r = app.session_txn(s, TxnRequest::atomic(vec![ /* ops */ ]), OpSource::Edit);
//!         Ok(match r { Some(_) => or_null(app.rebuild_slide(s, slide_index)), None => Null })
//!     })
//! }
//! ```
//!
//! Every handler wrapper ([`super::now`], [`super::later`], [`super::on`])
//! calls [`App::pump`] after the handler, which queues the flushes the
//! sessions scheduled (what `setImmediate` did).

use main_core::packages::i18n::Lang;
use main_core::slides::session_state::{
    DeckBroadcast, OpRecord, OpSource, Scheduled, SlideUpdate, plan_slide_update,
};
use shell_contract::ipc::Target;
use shell_contract::{IpcError, IpcValue, ViewId};

use super::engine::{DeckSize, TxnRequest, TxnResult};
use super::i18n::tm;
use super::state::{App, RenderSlide, SlideSession};
use crate::value::{as_num, as_str, field, is_nullish, num, obj, string};
use jscompat::JsonValue;

pub fn render_owner(
    cx: &shell_contract::ipc::CallContext,
) -> Result<super::engine::RenderOwner, IpcError> {
    let native = cx
        .native
        .as_ref()
        .ok_or_else(|| crate::value::thrown("missing authenticated render capability"))?;
    if !native
        .renderer()
        .is_some_and(|generation| generation.is_active())
    {
        return Err(crate::value::thrown(
            "renderer capability is missing or revoked",
        ));
    }
    Ok(super::engine::RenderOwner {
        view: cx.sender.view,
        context: native.clone(),
    })
}

/// `EMU_PER_PX_96`
pub const EMU_PER_PX_96: f64 = 9525.0;
/// `EMU_PER_PT`
pub const EMU_PER_PT: f64 = 12700.0;

/// `Math.round`: halves go up
pub fn js_round(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() {
        return x;
    }
    (x + 0.5).floor()
}

/// `op.key` of an argument the TypeScript reads without a guard: a read on
/// `null` or `undefined` throws a `TypeError`
pub fn prop<'a>(value: &'a IpcValue, key: &str) -> Result<&'a IpcValue, IpcError> {
    if is_nullish(value) {
        let what = if matches!(value, IpcValue::Null) {
            "null"
        } else {
            "undefined"
        };
        return Err(IpcError {
            name: Some("TypeError".into()),
            message: format!("Cannot read properties of {what} (reading '{key}')"),
        });
    }
    Ok(field(value, key))
}

pub fn null() -> Result<IpcValue, IpcError> {
    Ok(IpcValue::Null)
}

/// An object literal built key by key, with `...(cond ? { k: v } : {})`
#[derive(Default)]
pub struct Obj(Vec<(String, IpcValue)>);

impl Obj {
    pub fn new() -> Self {
        Obj::default()
    }

    /// An object continued: its keys first (anything else starts empty)
    pub fn from_value(value: IpcValue) -> Self {
        match value {
            IpcValue::Object(entries) => Obj(entries),
            _ => Obj::default(),
        }
    }

    pub fn set(mut self, key: &str, value: IpcValue) -> Self {
        self.0.push((key.to_owned(), value));
        self
    }

    pub fn set_if(self, cond: bool, key: &str, value: impl FnOnce() -> IpcValue) -> Self {
        if cond { self.set(key, value()) } else { self }
    }

    pub fn build(self) -> IpcValue {
        IpcValue::Object(self.0)
    }
}

/// `{ slide }` or `{ slide, el }`
pub fn target(slide: &IpcValue, el: Option<&IpcValue>) -> IpcValue {
    match el {
        Some(el) => obj([("slide", slide.clone()), ("el", el.clone())]),
        None => obj([("slide", slide.clone())]),
    }
}

/// `{ op: name, ...rest }`
pub fn op(name: &str) -> Obj {
    Obj::new().set("op", string(name))
}

/// `{ x, y, cx, cy }` of a box in px converted to EMU at `scale`
pub fn emu_box(scale: f64, x: f64, y: f64, w: f64, h: f64) -> IpcValue {
    let to_emu = |px: f64| js_round((px / scale) * EMU_PER_PX_96);
    obj([
        ("x", num(to_emu(x))),
        ("y", num(to_emu(y))),
        ("cx", num(to_emu(w))),
        ("cy", num(to_emu(h))),
    ])
}

/// A number read like `op.xPx` of a loosely typed argument; a non-number is
/// `NaN` as arithmetic would make it
pub fn number(value: &IpcValue) -> f64 {
    as_num(value).unwrap_or(f64::NAN)
}

pub fn array_of_strings(items: &[String]) -> IpcValue {
    IpcValue::Array(items.iter().map(|s| string(s.as_str())).collect())
}

impl App {
    pub fn lang(&self) -> Lang {
        self.locale.ui_lang()
    }

    /// `tm(key, params)`
    pub fn tm(&self, key: &str, params: &[(&str, JsonValue)]) -> String {
        tm(self.lang(), key, params)
    }

    pub fn emit(&self, view: ViewId, channel: &'static str, args: Vec<IpcValue>) {
        self.services.events.emit(Target::View(view), channel, args)
    }

    /// `setImmediate(job)`: runs on the core thread after the current
    /// handler has returned (and released the state). The job may be
    /// posted from any thread; it finds the app again on the core thread.
    pub fn post(&self, job: impl FnOnce(&App) + Send + 'static) {
        super::post_to(&self.services, self.registry_index, job)
    }

    /// `sessions.get(view)` of a handler that answers `null` without one
    pub fn with_session(
        &self,
        view: ViewId,
        f: impl FnOnce(&App, &mut SlideSession) -> Result<IpcValue, IpcError>,
    ) -> Result<IpcValue, IpcError> {
        let mut state = self.state.borrow_mut();
        match state.session_mut(view) {
            Some(session) => f(self, session),
            None => null(),
        }
    }

    /// `sessions.get(view)` of a handler that answers `default` without one
    pub fn with_session_or(
        &self,
        view: ViewId,
        default: IpcValue,
        f: impl FnOnce(&App, &mut SlideSession) -> Result<IpcValue, IpcError>,
    ) -> Result<IpcValue, IpcError> {
        let mut state = self.state.borrow_mut();
        match state.session_mut(view) {
            Some(session) => f(self, session),
            None => Ok(default),
        }
    }

    /// `{ cx, cy }` of the deck
    pub fn size_value(&self, size: DeckSize) -> IpcValue {
        obj([("cx", num(size.cx)), ("cy", num(size.cy))])
    }

    /// `rebuildSlide(session, slideIndex)`: `None` for a page the deck lacks
    pub fn rebuild_slide(
        &self,
        s: &SlideSession,
        owner: &super::engine::RenderOwner,
        index: &IpcValue,
    ) -> Result<Option<IpcValue>, IpcError> {
        let paths = self
            .engine
            .slide_paths(s.deck())
            .map_err(|error| crate::value::thrown(error.0))?;
        let Some(index) = slide_index(index, paths.len()) else {
            return Ok(None);
        };
        self.engine
            .render_slide(s.deck(), owner, index, s.fit_width_px, false)
            .map_err(|error| crate::value::thrown(error.0))
    }

    /// `rebuildSlideWithReparse(session, slideIndex)`
    pub fn rebuild_slide_with_reparse(
        &self,
        s: &SlideSession,
        owner: &super::engine::RenderOwner,
        index: &IpcValue,
    ) -> Result<Option<IpcValue>, IpcError> {
        let paths = self
            .engine
            .slide_paths(s.deck())
            .map_err(|error| crate::value::thrown(error.0))?;
        let Some(index) = slide_index(index, paths.len()) else {
            return Ok(None);
        };
        self.engine
            .render_slide(s.deck(), owner, index, s.fit_width_px, true)
            .map_err(|error| crate::value::thrown(error.0))
    }

    /// `buildAllRenderSlides(session.opened, session.fitWidthPx)`
    pub fn render_all(
        &self,
        s: &SlideSession,
        owner: &super::engine::RenderOwner,
    ) -> Result<IpcValue, IpcError> {
        self.engine
            .render_all(s.deck(), owner, s.fit_width_px)
            .map(IpcValue::Array)
            .map_err(|error| crate::value::thrown(error.0))
    }

    /// `session.opened.deck.slides.map((_, i) => rebuildSlide(session, i))`
    pub fn rebuild_every_slide(
        &self,
        s: &SlideSession,
        owner: &super::engine::RenderOwner,
    ) -> Result<IpcValue, IpcError> {
        let count = self
            .engine
            .slide_paths(s.deck())
            .map_err(|error| crate::value::thrown(error.0))?
            .len();
        let slides = (0..count)
            .map(|i| {
                self.engine
                    .render_slide(s.deck(), owner, i, s.fit_width_px, false)
                    .map(|slide| slide.unwrap_or(IpcValue::Null))
                    .map_err(|error| crate::value::thrown(error.0))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(IpcValue::Array(slides))
    }

    /// `journaledTxn(session, source, req)`: every applied batch lands in the
    /// session's op journal and schedules the deck broadcast
    pub fn journaled_txn(
        &self,
        s: &mut SlideSession,
        owner: &super::engine::RenderOwner,
        source: OpSource,
        request: &TxnRequest,
    ) -> Result<TxnResult, IpcError> {
        let result = self
            .engine
            .run_txn(s.deck(), owner, request)
            .map_err(|error| crate::value::thrown(error.0))?;
        if result.applied {
            let records = result
                .records
                .iter()
                .flatten()
                .map(|record| OpRecord {
                    op: record.op.clone(),
                    slide_id: record.slide_id.clone(),
                    created: record.created.clone(),
                })
                .collect();
            s.core.journal_ops(source, records);
            s.core.schedule_deck_broadcast();
        }
        Ok(result)
    }

    /// `sessionTxn(session, req, source)`: one undo step wrapping a
    /// transaction; planned first so an invalid request never touches
    /// history, and the step is erased when nothing applied
    pub fn session_txn(
        &self,
        s: &mut SlideSession,
        owner: &super::engine::RenderOwner,
        request: TxnRequest,
        source: OpSource,
    ) -> Result<Option<TxnResult>, IpcError> {
        let plan = self
            .engine
            .run_txn(s.deck(), owner, &request.dry())
            .map_err(|error| crate::value::thrown(error.0))?;
        let invalid = plan.failure_count();
        let refused = match request.isolation {
            super::engine::Isolation::Atomic => invalid > 0,
            super::engine::Isolation::PerOp => invalid >= request.ops.len(),
        };
        if refused {
            return Ok(None);
        }
        let snapshot = s
            .core
            .take_snapshot()
            .map_err(|error| crate::value::thrown(error.0))?;
        let result = self.journaled_txn(s, owner, source, &request)?;
        if !result.applied {
            return Ok(None);
        }
        s.core.push_history_snapshot(snapshot);
        Ok(Some(result))
    }

    /// `pushHistory(session)`, the transaction, and `undoStack.pop()` when it
    /// applied nothing: the shape of the shims that call `journaledTxn`
    /// themselves
    pub fn push_and_journal(
        &self,
        s: &mut SlideSession,
        owner: &super::engine::RenderOwner,
        source: OpSource,
        request: &TxnRequest,
    ) -> Result<Option<TxnResult>, IpcError> {
        let snapshot = s
            .core
            .take_snapshot()
            .map_err(|error| crate::value::thrown(error.0))?;
        let result = self.journaled_txn(s, owner, source, request)?;
        if !result.applied {
            return Ok(None);
        }
        s.core.push_history_snapshot(snapshot);
        Ok(Some(result))
    }

    /// `buildSlideUpdate(session, before, changed)`
    pub fn build_slide_update(
        &self,
        s: &mut SlideSession,
        owner: &super::engine::RenderOwner,
        before: &[String],
        changed: &[f64],
    ) -> Result<IpcValue, IpcError> {
        let refs = self
            .engine
            .slide_refs(s.deck())
            .map_err(|error| crate::value::thrown(error.0))?;
        let (order, rebuild) = plan_slide_update(&refs, before, changed);
        let slides: Vec<RenderSlide> = rebuild
            .iter()
            .map(|i| {
                self.engine
                    .render_slide(s.deck(), owner, *i, s.fit_width_px, false)
                    .map(|slide| RenderSlide(slide.unwrap_or(IpcValue::Null)))
                    .map_err(|error| crate::value::thrown(error.0))
            })
            .collect::<Result<_, _>>()?;
        let value = obj([
            ("order", array_of_strings(&order)),
            (
                "slides",
                IpcValue::Array(slides.iter().map(|r| r.0.clone()).collect()),
            ),
        ]);
        s.core.note_slide_update(SlideUpdate { order, slides });
        Ok(value)
    }

    /// The `{ path, slides, size, defaultFont }` an open answers
    pub fn open_result(&self, s: &SlideSession, slides: IpcValue) -> Result<IpcValue, IpcError> {
        let deck = s.deck();
        Ok(obj([
            ("path", string(s.path())),
            ("slides", slides),
            (
                "size",
                self.size_value(
                    self.engine
                        .size(deck)
                        .map_err(|error| crate::value::thrown(error.0))?,
                ),
            ),
            (
                "defaultFont",
                self.engine
                    .default_font(deck)
                    .map_err(|error| crate::value::thrown(error.0))?
                    .map_or(IpcValue::Undefined, string),
            ),
        ]))
    }

    /// `sessionDirty(session)`
    pub fn session_dirty(&self, s: &SlideSession) -> Result<bool, super::engine::EngineError> {
        if s.core.meta_dirty {
            return Ok(true);
        }
        self.engine.slides_dirty(s.deck())
    }

    /// Queues what the sessions scheduled with `setImmediate`, retaining the
    /// renderer generations that were attached when the work was queued.
    pub fn pump(&self) {
        let jobs = {
            let mut state = self.state.borrow_mut();
            let mut jobs = Vec::new();
            for slot in state.live_slots() {
                let scheduled = state
                    .slot_mut(slot)
                    .map(|session| session.core.take_scheduled())
                    .unwrap_or_default();
                let owners = state.attached_owners(slot);
                for item in scheduled {
                    jobs.push((slot, item, owners.clone()));
                }
            }
            jobs
        };
        super::export::owned::wake_generated_publications(self);
        for (slot, scheduled, owners) in jobs {
            self.post(move |app| app.flush(slot, scheduled, owners));
        }
    }

    fn flush(&self, slot: usize, scheduled: Scheduled, owners: Vec<super::engine::RenderOwner>) {
        let sends = {
            let mut state = self.state.borrow_mut();
            let owners: Vec<_> = owners
                .into_iter()
                .filter(|owner| {
                    state.slot_of(owner.view) == Some(slot)
                        && state
                            .owner_of(owner.view)
                            .and_then(|stored| {
                                stored.context.renderer().zip(owner.context.renderer())
                            })
                            .is_some_and(|(stored, captured)| {
                                captured.is_active() && stored.same_generation(captured)
                            })
                })
                .collect();
            let Some(session) = state.slot_mut(slot) else {
                return;
            };
            match scheduled {
                Scheduled::HistoryNotify => {
                    let changed = session.core.flush_history_notify();
                    let payload = obj([
                        ("canUndo", IpcValue::Bool(changed.can_undo)),
                        ("canRedo", IpcValue::Bool(changed.can_redo)),
                        ("aiRunActive", IpcValue::Bool(changed.ai_run_active)),
                    ]);
                    owners
                        .into_iter()
                        .map(|owner| (owner, "slides:history-changed", payload.clone()))
                        .collect::<Vec<_>>()
                }
                Scheduled::DeckBroadcast => {
                    let plan = session.core.flush_deck_broadcast(owners.len());
                    let size = match self.engine.size(session.deck()) {
                        Ok(size) => self.size_value(size),
                        Err(error) => {
                            eprintln!("[slides] deck broadcast failed: {}", error.0);
                            return;
                        }
                    };
                    let mut sends = Vec::new();
                    for owner in owners {
                        let payload = (|| -> Result<IpcValue, IpcError> {
                            Ok(match &plan {
                                DeckBroadcast::Nothing => return Ok(IpcValue::Undefined),
                                DeckBroadcast::Partial { order, slides } => {
                                    let indices: Option<Vec<usize>> = slides
                                        .iter()
                                        .map(|slide| {
                                            as_str(field(&slide.0, "partPath")).and_then(|path| {
                                                order.iter().position(|item| item == path)
                                            })
                                        })
                                        .collect();
                                    match indices {
                                        Some(indices) => {
                                            let rendered = indices
                                                .into_iter()
                                                .map(|index| {
                                                    self.engine
                                                        .render_slide(
                                                            session.deck(),
                                                            &owner,
                                                            index,
                                                            session.fit_width_px,
                                                            false,
                                                        )
                                                        .map(|slide| {
                                                            slide.unwrap_or(IpcValue::Null)
                                                        })
                                                        .map_err(|error| {
                                                            crate::value::thrown(error.0)
                                                        })
                                                })
                                                .collect::<Result<Vec<_>, _>>()?;
                                            obj([
                                                ("order", array_of_strings(order)),
                                                ("slides", IpcValue::Array(rendered)),
                                                ("size", size.clone()),
                                            ])
                                        }
                                        None => obj([
                                            ("slides", self.render_all(session, &owner)?),
                                            ("size", size.clone()),
                                        ]),
                                    }
                                }
                                DeckBroadcast::Full => obj([
                                    ("slides", self.render_all(session, &owner)?),
                                    ("size", size.clone()),
                                ]),
                            })
                        })();
                        match payload {
                            Ok(IpcValue::Undefined) => break,
                            Ok(payload) => sends.push((owner, "slides:deck-changed", payload)),
                            Err(error) => {
                                eprintln!("[slides] deck broadcast failed: {}", error.message)
                            }
                        }
                    }
                    sends
                }
            }
        };
        for (owner, channel, payload) in sends {
            if let Err(error) =
                self.services
                    .events
                    .emit_to_owner_checked(&owner, channel, vec![payload])
            {
                eprintln!("[slides] scoped broadcast delivery failed: {error}");
            }
        }
    }
}

/// `slides[index]` of an argument that may not index anything: JavaScript
/// reads the property named `String(index)`, so a number, a numeric string
/// and a one-element array of either index the same slide
pub fn slide_index(value: &IpcValue, len: usize) -> Option<usize> {
    if !matches!(
        value,
        IpcValue::Number(_) | IpcValue::String(_) | IpcValue::Array(_)
    ) {
        return None;
    }
    let key = js_string(value);
    let index: usize = key.parse().ok()?;
    (index.to_string() == key && index < len).then_some(index)
}

/// `String(value)` of a loosely typed argument
pub fn js_string(value: &IpcValue) -> String {
    match value {
        IpcValue::Undefined => "undefined".into(),
        IpcValue::Null => "null".into(),
        IpcValue::Bool(flag) => flag.to_string(),
        IpcValue::Number(x) => jscompat::number_to_string(*x),
        IpcValue::String(text) => text.clone(),
        IpcValue::Array(items) => items
            .iter()
            .map(|i| match i {
                IpcValue::Undefined | IpcValue::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        _ => "[object Object]".into(),
    }
}

#[cfg(test)]
mod created_record_tests {
    use main_core::slides::session_state::{Deck, OpRecord, OpSource, Session};
    use shell_contract::IpcValue;

    struct EmptyDeck;
    impl Deck<()> for EmptyDeck {
        type Error = std::convert::Infallible;
        fn take_snapshot(&self, _: bool) -> Result<(), Self::Error> {
            Ok(())
        }
        fn restore_snapshot(&mut self, _: ()) -> Result<bool, Self::Error> {
            Ok(false)
        }
    }

    #[test]
    fn render_owner_rejects_missing_and_revoked_capabilities() {
        use shell_contract::ipc::{CallContext, NativeContext, Sender};
        use shell_contract::runtime::{CancelToken, RendererGeneration};
        use shell_contract::{Surface, ViewId, WindowId};
        let generation = RendererGeneration::new();
        let mut context = CallContext {
            sender: Sender {
                view: ViewId(1),
                window: WindowId(1),
                surface: Surface::Slides,
            },
            request: None,
            cancel: CancelToken::default(),
            native: None,
        };
        assert!(super::render_owner(&context).is_err());
        context.native = Some(NativeContext::with_renderer((), generation.clone()));
        assert!(super::render_owner(&context).is_ok());
        generation.revoke();
        assert!(super::render_owner(&context).is_err());
    }

    #[test]
    fn journal_retains_undefined_created_items() {
        let mut session: Session<EmptyDeck, (), IpcValue, ()> = Session::new("", EmptyDeck);
        session.journal_ops(
            OpSource::Edit,
            vec![OpRecord {
                op: IpcValue::String("addSmartArt".into()),
                slide_id: None,
                created: Some(vec![
                    IpcValue::Undefined,
                    IpcValue::String("element".into()),
                ]),
            }],
        );
        let created = session
            .op_log
            .unwrap()
            .remove(0)
            .ops
            .remove(0)
            .created
            .unwrap();
        assert_eq!(
            created,
            vec![IpcValue::Undefined, IpcValue::String("element".into())]
        );
    }
}
