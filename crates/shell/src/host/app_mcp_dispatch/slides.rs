//! `SlidesControl` of the app MCP server: the visible-deck control that the TypeScript shell
//! builds in `apps/shell/src/main/mcp/slides-bridge.ts` (`createSlidesControl`).
//!
//! `readDeck`, `deckWithholds`, `runTxn` and `saveDeck` run the shared
//! [`SlidesBridge`] of main-core over the real slides editor of `shell_ipc_d1::slides`.
//! `openBlankTab` opens a blank deck tab and waits for the session its renderer creates.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jscompat::{JsObject, JsonValue, number_to_string};
use main_core::shell::mcp::mcp_server::{HostCall, Settled};
use main_core::shell::mcp::slides_bridge::{READY_TIMEOUT_MS, SlidesBridge, SlidesBridgeHost};
use main_core::shell::mcp::tools::slides_tools::ReadDeckOptions;
use pptx_engine::types::{ByteAnchor, Slide, SlideElement, TextElement};
use shell_contract::ipc::NativeEditorCommand;
use shell_contract::runtime::{CancelToken, Services};
use shell_contract::{Surface, ViewId};
use shell_ipc_d1::slides::SlidesViews;
use shell_ipc_w::tabs::TabsState;

use super::visible::{string, view_id};

/// the poll interval of the wait for a blank deck's session
const POLL: Duration = Duration::from_millis(100);

pub(super) fn dispatch(
    tabs: Rc<TabsState>,
    services: Services,
    call: HostCall,
    done: Box<dyn FnOnce(Settled)>,
) {
    let Some(views) = shell_ipc_d1::slides::views() else {
        return done(Err("the slides editor is unavailable".to_owned()));
    };
    if call.method == "openBlankTab" {
        return open_blank_tab(tabs, services, done);
    }
    // an MCP caller names a deck by its view id: only a live slides view is a deck
    let view = match view_id(call.args.first()).and_then(|id| {
        u32::try_from(id)
            .map(ViewId)
            .map_err(|_| "invalid document view id".to_owned())
    }) {
        Ok(view) => view,
        Err(error) => return done(Err(error)),
    };
    if !services
        .views
        .sender_of(view)
        .is_some_and(|sender| sender.surface == Surface::Slides)
    {
        return done(Err(
            "the deck view is closed or belongs to another editor".to_owned()
        ));
    }
    let wc_id = i64::from(view.0);
    let mut bridge = SlidesBridge::new(Host {
        views,
        services,
        last_error: None,
    });
    let answer = match call.method {
        "readDeck" => {
            let options = ReadDeckOptions {
                withhold: match call.args.get(1).and_then(|options| options.get("withhold")) {
                    Some(withhold) => match &*withhold {
                        JsonValue::Bool(flag) => Some(*flag),
                        _ => None,
                    },
                    None => None,
                },
            };
            bridge.read_deck(wc_id, Some(&options))
        }
        "deckWithholds" => bridge
            .deck_withholds(wc_id)
            .map(|withholds| Some(JsonValue::Bool(withholds))),
        "runTxn" => match call.args.get(1) {
            Some(request @ JsonValue::Object(_)) => {
                bridge.host_mut().last_error = None;
                let answer = bridge.run_txn(wc_id, request);
                // the bridge reads a failed transaction as a lost session; the editor's own
                // message is the better answer
                match bridge.host_mut().last_error.take() {
                    Some(error) if answer.is_err() => Err(error),
                    _ => answer,
                }
            }
            _ => Err("invalid slides transaction".to_owned()),
        },
        "saveDeck" => match (string(call.args.get(1)), call.args.get(2)) {
            (Ok(path), Some(JsonValue::Bool(overwrite))) => {
                bridge.save_deck(wc_id, path, *overwrite).map(|path| {
                    let mut saved = JsObject::new();
                    saved.set("path", JsonValue::String(path));
                    Some(JsonValue::Object(saved))
                })
            }
            _ => Err("invalid save arguments".to_owned()),
        },
        other => Err(format!("unknown SlidesControl.{other}")),
    };
    done(answer);
}

/// What the shared bridge needs of the world, answered by the slides editor of the shell.
/// The wait for a new tab's session (`open_blank_tab`, timers, `settle`) is the dispatcher's own
/// ([`open_blank_tab`]), which the bridge's synchronous host cannot drive.
struct Host {
    views: SlidesViews,
    services: Services,
    /// what the editor said when `apply_session_txn` failed; the host method can only answer
    /// "no session"
    last_error: Option<String>,
}

fn view_of(wc_id: i64) -> Option<ViewId> {
    u32::try_from(wc_id).ok().map(ViewId)
}

fn string_at(value: &JsonValue, key: &str) -> Option<String> {
    match value.get(key).as_deref() {
        Some(JsonValue::String(text)) => Some(text.clone()),
        _ => None,
    }
}

impl SlidesBridgeHost for Host {
    fn web_contents_alive(&mut self, wc_id: i64) -> bool {
        view_of(wc_id).is_some_and(|view| self.services.views.sender_of(view).is_some())
    }

    fn send_deck_changed(&mut self, _wc_id: i64, _payload: &JsonValue) {
        // `apply_session_txn` pushes the render to the tab itself and answers without it, so
        // the bridge has nothing left to push
    }

    fn now_ms(&mut self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
    }

    fn set_timeout(&mut self, _ms: f64) -> u64 {
        0
    }

    fn settle(&mut self, _op: u64, _outcome: Settled) {}

    fn open_blank_tab(&mut self) -> Result<i64, String> {
        Err("a blank deck tab is opened by the dispatcher".to_owned())
    }

    fn abandon_blank_tab(&mut self, _wc_id: i64) {}

    fn exists_sync(&mut self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    fn has_session(&mut self, wc_id: i64) -> bool {
        view_of(wc_id).is_some_and(|view| self.views.has_session(view))
    }

    fn deck(&mut self, wc_id: i64) -> Result<JsonValue, String> {
        let view = view_of(wc_id).ok_or("the deck session is gone")?;
        self.views
            .deck_model(view)
            .map_err(|error| error.message)
    }

    fn attached_ids(&mut self, wc_id: i64) -> Vec<i64> {
        view_of(wc_id)
            .map(|view| {
                self.views
                    .attached_views(view)
                    .into_iter()
                    .map(|attached| i64::from(attached.0))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn apply_session_txn(&mut self, wc_id: i64, req: &JsonValue) -> Option<JsonValue> {
        let view = view_of(wc_id)?;
        match self.views.apply_mcp_transaction(view, req) {
            Ok(result) => Some(result),
            Err(error) => {
                self.last_error = Some(error.message);
                None
            }
        }
    }

    fn save_session_deck_to(&mut self, wc_id: i64, path: &str) -> Result<(), String> {
        let view = view_of(wc_id).ok_or("the deck session is gone")?;
        self.views
            .save_to_path(view, path)
            .map_err(|error| error.message)
    }

    fn element_durable_id(&mut self, element: &JsonValue) -> Option<String> {
        // an element holding only what the identity reads: its bytes and its `nvId`
        let xml = element
            .get("anchor")
            .as_deref()
            .and_then(|anchor| string_at(anchor, "originalXml"))
            .filter(|xml| !xml.is_empty());
        let nv_id = match element.get("nvId").as_deref() {
            Some(JsonValue::String(text)) => Some(text.clone()),
            Some(JsonValue::Number(number)) => Some(number_to_string(*number)),
            _ => None,
        };
        let mut text = TextElement::default();
        if let Some(xml) = xml {
            text.anchor.set(ByteAnchor {
                sp_index: 0,
                original_xml: Arc::from(xml.as_str()),
                range: [0, 0],
                gap_after: None,
            });
        }
        if let Some(nv_id) = nv_id {
            text.nv_id.set(nv_id);
        }
        // an element whose bytes cannot be read has no durable id: the readout falls back to
        // its parse-time id
        pptx_engine::identity::element_durable_id(&SlideElement::Text(Box::new(text)))
            .ok()
            .flatten()
    }

    fn slide_durable_id(&mut self, slide: &JsonValue) -> String {
        let mut model = Slide::default();
        model.path.set(string_at(slide, "path").unwrap_or_default());
        pptx_engine::identity::slide_durable_id(&model)
    }

    fn slide_comments(&mut self, wc_id: i64, slide_path: &str) -> Result<Vec<JsonValue>, String> {
        let view = view_of(wc_id).ok_or("the deck session is gone")?;
        match self.views.slide_comments(view, slide_path) {
            Ok(JsonValue::Array(comments)) => Ok(comments),
            Ok(_) => Ok(Vec::new()),
            Err(error) => Err(error.message),
        }
    }

    fn slide_hidden(&mut self, slide: &JsonValue) -> bool {
        pptx_engine::generate::read_slide_hidden_xml(
            &string_at(slide, "bodyPrefix").unwrap_or_default(),
        )
    }
}

/// A deck tab that was opened and waits for its session: the renderer's boot pull creates it
/// a moment after the tab loads.
struct BlankWait {
    view: ViewId,
    deadline: Instant,
    tabs: Rc<TabsState>,
    services: Services,
    done: Box<dyn FnOnce(Settled)>,
}

thread_local! {
    static WAITS: RefCell<HashMap<u64, BlankWait>> = RefCell::new(HashMap::new());
    static NEXT_WAIT: Cell<u64> = const { Cell::new(0) };
}

fn open_blank_tab(tabs: Rc<TabsState>, services: Services, done: Box<dyn FnOnce(Settled)>) {
    let cancel = CancelToken::default();
    let operation = match crate::host::native_commands::issue(
        NativeEditorCommand::NewDocument {
            surface: Surface::Slides,
            directory: None,
        },
        cancel.clone(),
    ) {
        Ok(operation) => operation,
        Err(error) => return done(Err(error)),
    };
    let opened = tabs.clone();
    opened.open_mcp_blank_slides_native(
        operation,
        Box::new(move |result| {
            drop(cancel);
            match result {
                Ok(view) => {
                    let wait = BlankWait {
                        view,
                        deadline: Instant::now() + Duration::from_millis(READY_TIMEOUT_MS as u64),
                        tabs,
                        services,
                        done,
                    };
                    let id = NEXT_WAIT.with(|next| {
                        let id = next.get();
                        next.set(id + 1);
                        id
                    });
                    advance(id, wait);
                }
                Err(error) => done(Err(error)),
            }
        }),
    );
}

/// One pass of the wait: ready, closed, out of time, or asleep until the next poll
fn advance(id: u64, wait: BlankWait) {
    let ready = shell_ipc_d1::slides::views().is_some_and(|views| views.has_session(wait.view));
    let ended = if ready {
        Some(Ok(()))
    } else if wait.services.views.sender_of(wait.view).is_none() {
        Some(Err(
            "the presentation tab was closed before it became ready".to_owned()
        ))
    } else if Instant::now() > wait.deadline {
        Some(Err(format!(
            "the presentation did not become ready within {READY_TIMEOUT_MS}ms"
        )))
    } else {
        None
    };
    match ended {
        Some(Ok(())) => (wait.done)(Ok(Some(JsonValue::Number(f64::from(wait.view.0))))),
        Some(Err(message)) => {
            // a failed create_session leaves no orphan tab
            wait.tabs.abandon_mcp_blank_slides(wait.view);
            (wait.done)(Err(message));
        }
        None => {
            let workers = wait.services.workers.clone();
            let core = wait.services.core.clone();
            WAITS.with(|waits| waits.borrow_mut().insert(id, wait));
            workers.spawn(Box::new(move || {
                std::thread::sleep(POLL);
                core.post(Box::new(move || {
                    if let Some(wait) = WAITS.with(|waits| waits.borrow_mut().remove(&id)) {
                        advance(id, wait);
                    }
                }));
            }));
        }
    }
}
