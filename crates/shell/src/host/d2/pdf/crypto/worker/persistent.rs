//! Retained certificate-entry child. Only the supervisor owns termination/reaping.
use super::wire::{self, Kind};
use shell_contract::runtime::{CancelSubscription, CancelToken};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;
use zeroize::Zeroizing;

type Frames = Vec<Zeroizing<Vec<u8>>>;
type Answer = mpsc::Sender<Result<Vec<u8>, String>>;
type Guard = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;
type Admit = Box<dyn FnMut(Option<&[u8]>) -> Result<(), String> + Send>;
type Confirm = Arc<dyn Fn(String) -> Result<bool, String> + Send + Sync>;
enum Write {
    Frames(Frames),
    Reply(Kind),
    Stop,
}
enum Event {
    Frame(Result<(Kind, Vec<u8>), String>),
    WriterError,
    Confirmed(Result<bool, String>),
    Sign {
        frames: Frames,
        admit: Admit,
        answer: Answer,
    },
    Stop,
}

pub(super) struct Persistent {
    events: mpsc::Sender<Event>,
    alive: Arc<AtomicBool>,
    nonce: [u8; 32],
    entry_cancel: CancelToken,
    _entry_subscription: CancelSubscription,
}
impl Persistent {
    pub(super) fn open(
        frames: Frames,
        nonce: [u8; 32],
        guard: Guard,
        opening_cancel: &CancelToken,
        entry_cancel: &CancelToken,
        acquire: Admit,
        requires_acquire: bool,
        confirm: Confirm,
    ) -> Result<(Self, Vec<u8>), String> {
        guard()?;
        if opening_cancel.is_cancelled() || entry_cancel.is_cancelled() {
            return Err("signing request cancelled".into());
        }
        let mut worker = crate::index_worker_process::spawn_pdf_signer()?;
        let mut input = worker
            .child
            .stdin
            .take()
            .ok_or("signing worker input unavailable")?;
        let mut output = worker
            .child
            .stdout
            .take()
            .ok_or("signing worker output unavailable")?;
        let (events, receive) = mpsc::channel::<Event>();
        let (writes, write_rx) = mpsc::channel::<Write>();
        let (opened_tx, opened_rx) = mpsc::sync_channel(1);
        let alive = Arc::new(AtomicBool::new(true));
        let writer_events = events.clone();
        let writer = std::thread::spawn(move || {
            while let Ok(message) = write_rx.recv() {
                let result = match message {
                    Write::Frames(frames) => frames.into_iter().try_for_each(|frame| {
                        wire::write_frame(&mut input, Kind::Request, &frame, frame.len())
                    }),
                    Write::Reply(kind) => wire::write_frame(&mut input, kind, &nonce, nonce.len()),
                    Write::Stop => break,
                };
                if result.is_err() {
                    let _ = writer_events.send(Event::WriterError);
                    break;
                }
            }
        });
        let reader_events = events.clone();
        let reader = std::thread::spawn(move || {
            loop {
                let frame = wire::read_streamed_frame(&mut output);
                let finished = frame.is_err();
                if reader_events.send(Event::Frame(frame)).is_err() || finished {
                    break;
                }
            }
        });
        let stop = events.clone();
        let subscription = entry_cancel.on_cancel(move || {
            let _ = stop.send(Event::Stop);
        });
        let open_stop = events.clone();
        let opening_subscription = opening_cancel.on_cancel(move || {
            let _ = open_stop.send(Event::Stop);
        });
        let active = alive.clone();
        let revoke = entry_cancel.clone();
        let interaction_events = events.clone();
        std::thread::spawn(move || {
            let mut opening = Some(opened_tx);
            let mut pending: Option<Answer> = None;
            let mut signed: Option<Vec<u8>> = None;
            let mut admitted = 0u8;
            let mut opening_admit = acquire;
            let mut confirming = false;
            let mut sign_admit: Option<Admit> = None;
            let _ = writes.send(Write::Frames(frames));
            let failure = loop {
                if revoke.is_cancelled() {
                    break "signing request cancelled".to_owned();
                }
                if guard().is_err() {
                    break "signing request revoked".to_owned();
                }
                let event = match receive.recv_timeout(Duration::from_millis(25)) {
                    Ok(event) => event,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        break "signing worker disconnected".to_owned();
                    }
                };
                match event {
                    Event::Stop => break "signing request cancelled".to_owned(),
                    Event::WriterError => break "signing worker input closed".to_owned(),
                    Event::Confirmed(result) if confirming && opening.is_some() => {
                        confirming = false;
                        let accepted = matches!(result, Ok(true));
                        if accepted {
                            if guard().is_err() {
                                break "signing request revoked".to_owned();
                            }
                            if let Err(error) = opening_admit(None) {
                                break error;
                            }
                        }
                        let reply = if accepted {
                            Kind::ConfirmAccept
                        } else {
                            Kind::ConfirmDeny
                        };
                        if writes.send(Write::Reply(reply)).is_err() {
                            break "signing worker input closed".to_owned();
                        }
                    }
                    Event::Confirmed(_) => break "invalid signing worker protocol".to_owned(),
                    Event::Sign {
                        frames,
                        admit,
                        answer,
                    } if opening.is_none() && pending.is_none() => {
                        admitted = 0;
                        sign_admit = Some(admit);
                        pending = Some(answer);
                        if writes.send(Write::Frames(frames)).is_err() {
                            break "signing worker input closed".to_owned();
                        }
                    }
                    Event::Sign { answer, .. } => {
                        let _ = answer.send(Err("signing session already used".into()));
                    }
                    Event::Frame(Err(error)) => {
                        break if error == "signing frame allocation failed"
                            || error == "signing frame length overflow"
                        {
                            error
                        } else {
                            "signing worker disconnected".to_owned()
                        };
                    }
                    Event::Frame(Ok((kind, body))) => {
                        if body.len() < nonce.len() || body[..nonce.len()] != nonce {
                            break "invalid signing response".to_owned();
                        }
                        let body = &body[nonce.len()..];
                        match kind {
                            Kind::Guard if body.is_empty() => {
                                if guard().is_err()
                                    || writes.send(Write::Reply(Kind::Permit)).is_err()
                                {
                                    break "signing request revoked".to_owned();
                                }
                            }
                            Kind::ConfirmToken
                                if opening.is_some()
                                    && requires_acquire
                                    && admitted == 1
                                    && !confirming =>
                            {
                                let Ok(name) = std::str::from_utf8(body) else {
                                    break "invalid signing worker protocol".to_owned();
                                };
                                if guard().is_err() {
                                    break "signing request revoked".to_owned();
                                }
                                confirming = true;
                                let name = name.to_owned();
                                let confirm = confirm.clone();
                                let events = interaction_events.clone();
                                std::thread::spawn(move || {
                                    let _ = events.send(Event::Confirmed(confirm(name)));
                                });
                            }
                            Kind::AdmitFirst if body.is_empty() && admitted == 0 => {
                                let check = if opening.is_some() {
                                    opening_admit(None)
                                } else if let Some(admit) = sign_admit.as_mut() {
                                    admit(None)
                                } else {
                                    Err("unexpected signing admission".into())
                                };
                                if let Err(error) = check {
                                    break error;
                                }
                                admitted = 1;
                                if writes.send(Write::Reply(Kind::Permit)).is_err() {
                                    break "signing worker input closed".to_owned();
                                }
                            }
                            Kind::AdmitRetry
                                if !body.is_empty() && opening.is_none() && admitted == 1 =>
                            {
                                let Some(admit) = sign_admit.as_mut() else {
                                    break "unexpected signing admission".to_owned();
                                };
                                if let Err(error) = admit(Some(body)) {
                                    break error;
                                }
                                admitted = 2;
                                if writes.send(Write::Reply(Kind::Permit)).is_err() {
                                    break "signing worker input closed".to_owned();
                                }
                            }
                            Kind::Opened
                                if opening.is_some()
                                    && !confirming
                                    && admitted == u8::from(requires_acquire) =>
                            {
                                if guard().is_err() {
                                    break "signing request revoked".to_owned();
                                }
                                if let Some(answer) = opening.take() {
                                    let _ = answer.send(Ok(body.to_vec()));
                                }
                            }
                            Kind::Signed if pending.is_some() && admitted > 0 => {
                                if guard().is_err() {
                                    break "signing request revoked".to_owned();
                                }
                                signed = Some(body.to_vec());
                                break "signing session complete".to_owned();
                            }
                            Kind::OpenMessage if opening.is_some() => {
                                break match body {
                                    [1] => "open-regex-limit",
                                    [2] => "open-worker-unavailable",
                                    [3] => "signing request revoked",
                                    [4] => "signing frame allocation failed",
                                    _ => "invalid signing worker protocol",
                                }
                                .to_owned();
                            }
                            Kind::Error => {
                                let code = std::str::from_utf8(body).unwrap_or("");
                                break if let Some(code) =
                                    shell_ipc_d2::pdf::editor::hosts::CertificateOpenCode::parse(
                                        code,
                                    ) {
                                    code.as_str().to_owned()
                                } else if code == "pdf-encrypted" && opening.is_none() {
                                    code.to_owned()
                                } else {
                                    "sign-failed".to_owned()
                                };
                            }
                            _ => break "invalid signing worker protocol".to_owned(),
                        }
                    }
                }
            };
            active.store(false, Ordering::Release);
            if signed.is_none() {
                revoke.cancel();
            }
            let _ = writes.send(Write::Stop);
            let cleanup = worker.terminate();
            let _ = writer.join();
            let _ = reader.join();
            let error = cleanup.err();
            if error.is_some() {
                revoke.cancel();
            }
            if let Some(answer) = opening {
                let _ = answer.send(Err(error.clone().unwrap_or_else(|| failure.clone())));
            }
            if let Some(answer) = pending {
                let result = match (signed, error) {
                    (_, Some(error)) => Err(error),
                    (Some(bytes), None) => Ok(bytes),
                    (None, None) => Err(failure),
                };
                let _ = answer.send(result);
            }
        });
        let result = opened_rx
            .recv()
            .map_err(|_| "signing worker disconnected")?;
        drop(opening_subscription);
        let session = Self {
            events,
            alive,
            nonce,
            entry_cancel: entry_cancel.clone(),
            _entry_subscription: subscription,
        };
        match result {
            Ok(opened) => Ok((session, opened)),
            Err(error) => Err(error),
        }
    }
    pub(super) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire) && !self.entry_cancel.is_cancelled()
    }
    pub(super) fn nonce(&self) -> [u8; 32] {
        self.nonce
    }
    pub(super) fn sign(
        &mut self,
        frames: Frames,
        admit: Admit,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>, String> {
        if !self.is_alive() || cancel.is_cancelled() {
            return Err("signing session is closed".into());
        }
        let (answer, result) = mpsc::channel();
        let stop = self.events.clone();
        let _cancel = cancel.on_cancel(move || {
            let _ = stop.send(Event::Stop);
        });
        self.events
            .send(Event::Sign {
                frames,
                admit,
                answer,
            })
            .map_err(|_| "signing session is closed")?;
        result.recv().map_err(|_| "signing worker disconnected")?
    }
}
impl Drop for Persistent {
    fn drop(&mut self) {
        let _ = self.events.send(Event::Stop);
    }
}
