//! The output stream of each view (the stage 3 shell contract, section 2,
//! "order"): replies and events leave through one Tauri `ipc::Channel` per
//! view, which delivers them in the order they were sent. The bridge opens it
//! with the command `ipc_listen` when a page starts; a frame sent before that
//! waits here.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use shell_contract::ids::ViewId;
use shell_contract::runtime::RendererGeneration;
use shell_core::outlet::Outlet;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::trace::trace;

/// What waits for a stream that is not open yet is bounded: a view that never
/// listens must not grow the queue without end
const MAX_WAITING_FRAMES: usize = 1024;

struct Attached {
    channel: Arc<Channel<InvokeResponseBody>>,
    generation: RendererGeneration,
}

struct QueuedFrame {
    generation: Option<RendererGeneration>,
    destination: Option<Arc<Attached>>,
    bytes: Vec<u8>,
}

struct IndeterminateDelivery {
    generation: RendererGeneration,
    error: String,
}

#[derive(Default)]
struct StreamState {
    channel: Option<Arc<Attached>>,
    queue: VecDeque<QueuedFrame>,
    draining: bool,
    forgotten: bool,
    /// A failed Channel::send consumed a channel index and may have delivered.
    /// Later frames must not cross that unresolved ordering gap.
    indeterminate: Option<IndeterminateDelivery>,
}

#[derive(Default)]
struct StreamCell(Mutex<StreamState>);

/// A claimed single-stream drainer. It must run only after every admission gate drops.
#[must_use]
pub(crate) struct DrainTicket {
    stream: Arc<StreamCell>,
    view: ViewId,
    armed: bool,
    deferred_frames: Vec<QueuedFrame>,
    deferred_channel: Option<Arc<Attached>>,
    initial_flush: Option<(Arc<Attached>, usize)>,
}

impl DrainTicket {
    pub(crate) fn drain(mut self) -> Result<(), String> {
        if !self.armed {
            return Ok(());
        }
        loop {
            let next = {
                let mut state = self
                    .stream
                    .0
                    .lock()
                    .map_err(|_| "event stream lock poisoned".to_owned())?;
                if state.forgotten
                    || state
                        .queue
                        .front()
                        .is_none_or(|frame| frame.destination.is_none())
                {
                    state.draining = false;
                    self.armed = false;
                    break;
                }
                state.queue.pop_front()
            };
            let Some(frame) = next else {
                continue;
            };
            let Some(destination) = frame.destination else {
                continue;
            };
            let initial = self
                .initial_flush
                .as_mut()
                .is_some_and(|(channel, remaining)| {
                    if *remaining == 0 || !Arc::ptr_eq(channel, &destination) {
                        return false;
                    }
                    *remaining -= 1;
                    true
                });
            if let Err(error) = destination
                .channel
                .send(InvokeResponseBody::Raw(frame.bytes))
            {
                let error = format!(
                    "indeterminate event delivery for view {}: {error}",
                    self.view.0
                );
                if initial {
                    self.fail_initial_attachment(&destination);
                    self.initial_flush = None;
                } else {
                    let mut state = self
                        .stream
                        .0
                        .lock()
                        .map_err(|_| "event stream lock poisoned".to_owned())?;
                    state.indeterminate = Some(IndeterminateDelivery {
                        generation: destination.generation.clone(),
                        error: error.clone(),
                    });
                    state.draining = false;
                    self.armed = false;
                }
                return Err(error);
            }
            if self
                .initial_flush
                .as_ref()
                .is_some_and(|(_, remaining)| *remaining == 0)
            {
                self.initial_flush = None;
            }
        }
        Ok(())
    }

    fn fail_initial_attachment(&mut self, failed: &Arc<Attached>) {
        let (discarded, channel) = {
            let mut state = self
                .stream
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut discarded = Vec::new();
            for frame in std::mem::take(&mut state.queue) {
                if frame
                    .destination
                    .as_ref()
                    .is_some_and(|destination| Arc::ptr_eq(destination, failed))
                {
                    discarded.push(frame);
                } else {
                    state.queue.push_back(frame);
                }
            }
            let channel = if state
                .channel
                .as_ref()
                .is_some_and(|channel| Arc::ptr_eq(channel, failed))
            {
                state.channel.take()
            } else {
                None
            };
            (discarded, channel)
        };
        for _ in &discarded {
            eprintln!(
                "shell: view {}: a queued frame was not sent because stream attachment failed",
                self.view.0
            );
        }
        drop((discarded, channel));
    }
}

impl Drop for DrainTicket {
    fn drop(&mut self) {
        if self.armed {
            self.stream
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .draining = false;
        }
    }
}

#[derive(Default)]
pub struct TauriOutlet {
    streams: Mutex<HashMap<ViewId, Arc<StreamCell>>>,
}

impl TauriOutlet {
    pub fn expect(&self, view: ViewId) {
        self.streams
            .lock()
            .unwrap()
            .entry(view)
            .or_insert_with(|| Arc::new(StreamCell::default()));
    }

    fn stream(&self, view: ViewId) -> Result<Arc<StreamCell>, String> {
        self.streams
            .lock()
            .map_err(|_| "event stream lock poisoned".to_owned())?
            .get(&view)
            .cloned()
            .ok_or_else(|| format!("view {} is gone", view.0))
    }

    fn claim(
        stream: &Arc<StreamCell>,
        state: &mut StreamState,
        view: ViewId,
    ) -> Option<DrainTicket> {
        if state.indeterminate.is_some()
            || state.draining
            || state
                .queue
                .front()
                .is_none_or(|frame| frame.destination.is_none())
        {
            return None;
        }
        state.draining = true;
        Some(DrainTicket {
            stream: stream.clone(),
            view,
            armed: true,
            deferred_frames: Vec::new(),
            deferred_channel: None,
            initial_flush: None,
        })
    }

    pub fn attach(
        &self,
        view: ViewId,
        channel: Channel<InvokeResponseBody>,
        generation: RendererGeneration,
    ) -> Result<bool, String> {
        let channel = Arc::new(channel);
        let retained = channel.clone();
        let (replaced, drain) = generation
            .with_active(|| self.attach_generation_fenced(view, channel, &generation))
            .unwrap_or_else(|| Err("renderer generation was revoked".into()))?;
        drop(retained);
        if let Some(drain) = drain {
            drain.drain()?;
        }
        Ok(replaced)
    }

    /// Caller holds the generation gate; draining happens after the gate is released.
    pub(crate) fn attach_generation_fenced(
        &self,
        view: ViewId,
        channel: Arc<Channel<InvokeResponseBody>>,
        generation: &RendererGeneration,
    ) -> Result<(bool, Option<DrainTicket>), String> {
        let stream = self.stream(view)?;
        let mut state = stream
            .0
            .lock()
            .map_err(|_| "event stream lock poisoned".to_owned())?;
        if state.forgotten {
            return Err(format!("view {} is gone", view.0));
        }
        if let Some(failure) = &state.indeterminate {
            return Err(failure.error.clone());
        }
        let replaced = state.channel.is_some();
        if let Some(current) = &state.channel {
            if current.generation.is_active() && !current.generation.same_generation(generation) {
                return Err("another renderer owns the event stream".into());
            }
        }
        let attached = Arc::new(Attached {
            channel,
            generation: generation.clone(),
        });
        let mut discarded = Vec::new();
        let mut initial_count = 0;
        let queued = std::mem::take(&mut state.queue);
        for mut frame in queued {
            if frame.destination.is_none()
                && frame
                    .generation
                    .as_ref()
                    .is_some_and(|queued| !queued.same_generation(generation))
            {
                discarded.push(frame);
                continue;
            }
            if frame.destination.is_none() {
                frame.generation = Some(generation.clone());
                frame.destination = Some(attached.clone());
                initial_count += 1;
            }
            state.queue.push_back(frame);
        }
        let previous = state.channel.replace(attached.clone());
        let mut drain = Self::claim(&stream, &mut state, view).unwrap_or_else(|| DrainTicket {
            stream: stream.clone(),
            view,
            armed: false,
            deferred_frames: Vec::new(),
            deferred_channel: None,
            initial_flush: None,
        });
        drain.deferred_frames = discarded;
        drain.deferred_channel = previous;
        if !replaced && drain.armed && initial_count > 0 {
            drain.initial_flush = Some((attached, initial_count));
        }
        Ok((replaced, Some(drain)))
    }

    /// Caller holds the generation gate; this only enqueues and never sends.
    pub(crate) fn enqueue_generation_fenced(
        &self,
        view: ViewId,
        generation: &RendererGeneration,
        frame: Vec<u8>,
    ) -> Result<Option<DrainTicket>, String> {
        let stream = self.stream(view)?;
        let mut state = stream
            .0
            .lock()
            .map_err(|_| "event stream lock poisoned".to_owned())?;
        if state.forgotten {
            return Err(format!("view {} is gone", view.0));
        }
        if let Some(failure) = &state.indeterminate {
            return Err(if failure.generation.same_generation(generation) {
                failure.error.clone()
            } else {
                format!(
                    "{}; generation-bound recovery has not been acknowledged",
                    failure.error
                )
            });
        }
        if state.channel.is_none() && state.queue.len() >= MAX_WAITING_FRAMES {
            return Err(format!("view {} event queue is full", view.0));
        }
        let destination = match &state.channel {
            Some(channel) if channel.generation.same_generation(generation) => {
                Some(channel.clone())
            }
            Some(_) => return Err("renderer generation does not own the event stream".into()),
            None => None,
        };
        state.queue.push_back(QueuedFrame {
            generation: Some(generation.clone()),
            destination,
            bytes: frame,
        });
        Ok(Self::claim(&stream, &mut state, view))
    }

    pub fn forget(&self, view: ViewId) {
        let stream = self.streams.lock().unwrap().remove(&view);
        if let Some(stream) = stream {
            let (queued, channel) = {
                let mut state = stream.0.lock().unwrap_or_else(|error| error.into_inner());
                state.forgotten = true;
                (std::mem::take(&mut state.queue), state.channel.take())
            };
            drop((queued, channel));
        }
    }
}

impl Outlet for TauriOutlet {
    fn deliver(&self, view: ViewId, frame: Vec<u8>) {
        let Ok(stream) = self.stream(view) else {
            trace!(
                "outlet",
                "view {}: gone, {} bytes dropped",
                view.0,
                frame.len()
            );
            return;
        };
        let drain = {
            let mut state = stream.0.lock().unwrap_or_else(|error| error.into_inner());
            if state.forgotten {
                return;
            }
            if let Some(failure) = &state.indeterminate {
                eprintln!("shell: {}", failure.error);
                if state.queue.len() < MAX_WAITING_FRAMES {
                    let destination = state.channel.clone();
                    let generation = destination
                        .as_ref()
                        .map(|channel| channel.generation.clone());
                    state.queue.push_back(QueuedFrame {
                        generation,
                        destination,
                        bytes: frame,
                    });
                } else {
                    eprintln!("shell: view {}: blocked event queue is full", view.0);
                }
                return;
            }
            if state.channel.is_none() && state.queue.len() >= MAX_WAITING_FRAMES {
                eprintln!(
                    "shell: view {}: event queue is full, a frame was dropped",
                    view.0
                );
                return;
            }
            let destination = state.channel.clone();
            let generation = destination
                .as_ref()
                .map(|channel| channel.generation.clone());
            state.queue.push_back(QueuedFrame {
                generation,
                destination,
                bytes: frame,
            });
            Self::claim(&stream, &mut state, view)
        };
        if let Some(drain) = drain {
            let _ = drain.drain();
        }
    }

    fn deliver_to_generation_checked(
        &self,
        view: ViewId,
        generation: &RendererGeneration,
        frame: Vec<u8>,
    ) -> Result<(), String> {
        let drain = generation
            .with_active(|| self.enqueue_generation_fenced(view, generation, frame))
            .unwrap_or_else(|| Err("renderer generation was revoked".into()))?;
        if let Some(drain) = drain {
            drain.drain()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_delivery_reports_missing_full_and_revoked_streams() {
        let outlet = TauriOutlet::default();
        let view = ViewId(42);
        let generation = RendererGeneration::new();
        assert!(
            outlet
                .deliver_to_generation_checked(view, &generation, vec![1])
                .is_err()
        );
        outlet.expect(view);
        for _ in 0..MAX_WAITING_FRAMES {
            assert!(
                outlet
                    .deliver_to_generation_checked(view, &generation, vec![1])
                    .is_ok()
            );
        }
        assert!(
            outlet
                .deliver_to_generation_checked(view, &generation, vec![1])
                .is_err()
        );
        generation.revoke();
        assert!(
            outlet
                .deliver_to_generation_checked(view, &generation, vec![1])
                .is_err()
        );
        outlet.forget(view);
        assert!(
            outlet
                .deliver_to_generation_checked(view, &RendererGeneration::new(), vec![1])
                .is_err()
        );
    }

    #[test]
    fn scoped_frames_never_cross_renderer_replacement() {
        let outlet = TauriOutlet::default();
        let view = ViewId(43);
        let old = RendererGeneration::new();
        let next = RendererGeneration::new();
        let received = std::sync::Arc::new(Mutex::new(Vec::new()));
        outlet.expect(view);
        outlet
            .deliver_to_generation_checked(view, &old, vec![1])
            .unwrap();
        old.revoke();
        outlet
            .deliver_to_generation_checked(view, &next, vec![2])
            .unwrap();
        let capture = received.clone();
        let channel = Channel::new(move |body| {
            if let InvokeResponseBody::Raw(bytes) = body {
                capture.lock().unwrap().push(bytes);
            }
            Ok(())
        });
        assert!(!outlet.attach(view, channel, next.clone()).unwrap());
        assert_eq!(*received.lock().unwrap(), vec![vec![2]]);
        assert!(
            outlet
                .deliver_to_generation_checked(view, &old, vec![3])
                .is_err()
        );
        let unrelated = RendererGeneration::new();
        assert!(
            outlet
                .attach(view, Channel::new(|_| Ok(())), unrelated)
                .is_err()
        );
        outlet
            .deliver_to_generation_checked(view, &next, vec![4])
            .unwrap();
        assert_eq!(*received.lock().unwrap(), vec![vec![2], vec![4]]);
    }

    #[test]
    fn revocation_does_not_wait_for_channel_send_after_enqueue() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;

        let outlet = Arc::new(TauriOutlet::default());
        let view = ViewId(44);
        let generation = RendererGeneration::new();
        outlet.expect(view);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let channel = Channel::new(move |_| {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            Ok(())
        });
        outlet.attach(view, channel, generation.clone()).unwrap();
        let sending_outlet = outlet.clone();
        let sending_generation = generation.clone();
        let sender = std::thread::spawn(move || {
            sending_outlet.deliver_to_generation_checked(view, &sending_generation, vec![1])
        });
        entered_rx.recv().unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (revoked_tx, revoked_rx) = mpsc::channel();
        let revoking_generation = generation.clone();
        let revoker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            revoking_generation.revoke();
            revoked_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(revoked_rx.recv_timeout(Duration::from_secs(1)).is_ok());
        release_tx.send(()).unwrap();
        sender.join().unwrap().unwrap();
        revoker.join().unwrap();
        assert!(
            outlet
                .deliver_to_generation_checked(view, &generation, vec![2])
                .is_err()
        );
    }

    #[test]
    fn reentrant_send_enqueues_after_current_frame_without_locking_callback() {
        let outlet = Arc::new(TauriOutlet::default());
        let weak = Arc::downgrade(&outlet);
        let view = ViewId(45);
        let generation = RendererGeneration::new();
        let callback_generation = generation.clone();
        let received = Arc::new(Mutex::new(Vec::new()));
        let seen = received.clone();
        outlet.expect(view);
        let channel = Channel::new(move |body| {
            if let InvokeResponseBody::Raw(bytes) = body {
                seen.lock().unwrap().push(bytes.clone());
                if bytes == [1] {
                    weak.upgrade()
                        .unwrap()
                        .deliver_to_generation_checked(view, &callback_generation, vec![2])
                        .unwrap();
                }
            }
            Ok(())
        });
        outlet.attach(view, channel, generation.clone()).unwrap();
        outlet
            .deliver_to_generation_checked(view, &generation, vec![1])
            .unwrap();
        assert_eq!(*received.lock().unwrap(), vec![vec![1], vec![2]]);
    }

    #[test]
    fn reentrant_forget_drops_remaining_frames_without_retargeting() {
        let outlet = Arc::new(TauriOutlet::default());
        let weak = Arc::downgrade(&outlet);
        let view = ViewId(46);
        let generation = RendererGeneration::new();
        outlet.expect(view);
        let channel = Channel::new(move |_| {
            weak.upgrade().unwrap().forget(view);
            Ok(())
        });
        outlet.attach(view, channel, generation.clone()).unwrap();
        outlet
            .deliver_to_generation_checked(view, &generation, vec![1])
            .unwrap();
        assert!(
            outlet
                .deliver_to_generation_checked(view, &generation, vec![2])
                .is_err()
        );
    }

    #[test]
    fn accepted_old_frames_never_move_to_a_replacement_channel() {
        use std::sync::mpsc;
        let outlet = Arc::new(TauriOutlet::default());
        let view = ViewId(47);
        let old = RendererGeneration::new();
        let new = RendererGeneration::new();
        let old_frames = Arc::new(Mutex::new(Vec::new()));
        let new_frames = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let sink = old_frames.clone();
        outlet.expect(view);
        outlet
            .attach(
                view,
                Channel::new(move |body| {
                    if let InvokeResponseBody::Raw(bytes) = body {
                        sink.lock().unwrap().push(bytes.clone());
                        if bytes == [1] {
                            entered_tx.send(()).unwrap();
                            release_rx.lock().unwrap().recv().unwrap();
                        }
                    }
                    Ok(())
                }),
                old.clone(),
            )
            .unwrap();
        let sender = outlet.clone();
        let generation = old.clone();
        let thread = std::thread::spawn(move || {
            sender.deliver_to_generation_checked(view, &generation, vec![1])
        });
        entered_rx.recv().unwrap();
        outlet
            .deliver_to_generation_checked(view, &old, vec![2])
            .unwrap();
        old.revoke();
        let sink = new_frames.clone();
        outlet
            .attach(
                view,
                Channel::new(move |body| {
                    if let InvokeResponseBody::Raw(bytes) = body {
                        sink.lock().unwrap().push(bytes);
                    }
                    Ok(())
                }),
                new.clone(),
            )
            .unwrap();
        outlet
            .deliver_to_generation_checked(view, &new, vec![3])
            .unwrap();
        release_tx.send(()).unwrap();
        thread.join().unwrap().unwrap();
        assert_eq!(*old_frames.lock().unwrap(), vec![vec![1], vec![2]]);
        assert_eq!(*new_frames.lock().unwrap(), vec![vec![3]]);
    }

    #[test]
    fn failed_initial_flush_reports_error_and_does_not_attach_stream() {
        let outlet = TauriOutlet::default();
        let view = ViewId(48);
        let generation = RendererGeneration::new();
        outlet.expect(view);
        outlet
            .deliver_to_generation_checked(view, &generation, vec![1])
            .unwrap();
        outlet
            .deliver_to_generation_checked(view, &generation, vec![2])
            .unwrap();
        let failed = Channel::new(|_| Err(std::io::Error::other("send failed").into()));
        assert!(outlet.attach(view, failed, generation.clone()).is_err());
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = received.clone();
        assert!(
            !outlet
                .attach(
                    view,
                    Channel::new(move |body| {
                        if let InvokeResponseBody::Raw(bytes) = body {
                            sink.lock().unwrap().push(bytes);
                        }
                        Ok(())
                    }),
                    generation.clone()
                )
                .unwrap()
        );
        assert!(received.lock().unwrap().is_empty());
        outlet
            .deliver_to_generation_checked(view, &generation, vec![3])
            .unwrap();
        assert_eq!(*received.lock().unwrap(), vec![vec![3]]);
    }

    #[test]
    fn indeterminate_send_stops_ordered_drain_without_replaying_successes() {
        let outlet = Arc::new(TauriOutlet::default());
        let weak = Arc::downgrade(&outlet);
        let view = ViewId(49);
        let generation = RendererGeneration::new();
        let callback_generation = generation.clone();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let seen = observed.clone();
        outlet.expect(view);
        outlet
            .attach(
                view,
                Channel::new(move |body| {
                    let InvokeResponseBody::Raw(bytes) = body else {
                        panic!("expected raw frame")
                    };
                    seen.lock().unwrap().push(bytes.clone());
                    if bytes == [2] {
                        weak.upgrade()
                            .unwrap()
                            .deliver_to_generation_checked(view, &callback_generation, vec![3])
                            .unwrap();
                        return Err(std::io::Error::other("delivery uncertain").into());
                    }
                    Ok(())
                }),
                generation.clone(),
            )
            .unwrap();
        outlet
            .deliver_to_generation_checked(view, &generation, vec![1])
            .unwrap();
        let error = outlet
            .deliver_to_generation_checked(view, &generation, vec![2])
            .unwrap_err();
        assert!(error.contains("indeterminate event delivery"));
        assert_eq!(*observed.lock().unwrap(), vec![vec![1], vec![2]]);
        assert!(
            outlet
                .deliver_to_generation_checked(view, &generation, vec![4])
                .is_err()
        );
        assert!(
            outlet
                .attach(view, Channel::new(|_| Ok(())), generation.clone())
                .is_err()
        );
        let stream = outlet.stream(view).unwrap();
        let state = stream.0.lock().unwrap();
        assert_eq!(state.queue.len(), 1);
        assert_eq!(state.queue.front().unwrap().bytes, vec![3]);
    }
}
