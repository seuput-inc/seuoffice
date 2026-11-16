//! A delegated Wayland source is an explicitly selected portal session, not an XID.
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::io::Cursor;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::task::Poll;
use std::time::{Duration, Instant};

use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use image::RgbaImage;
use pipewire as pw;
use pw::spa::{
    self,
    param::{
        ParamType,
        format::{FormatProperties, MediaSubtype, MediaType},
        format_utils,
        video::{VideoFormat, VideoInfoRaw},
    },
    pod::{Pod, serialize::PodSerializer},
    utils::{Direction, Fraction, Rectangle, SpaTypes},
};
use shell_contract::{
    ipc::NativeRequestOwner,
    runtime::{CancelSubscription, CancelToken},
};
use shell_ipc_d1::sheets::engine::{ScreenImage, ScreenSource};

use super::capture;
use crate::ipc::native_owner;

pub(super) fn enabled() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|value| value == "wayland")
}

type FrameDone = mpsc::SyncSender<Result<RgbaImage, String>>;
enum Command {
    Capture {
        cancel: CancelToken,
        size: (u32, u32),
        done: FrameDone,
    },
    Close,
}
struct Selection {
    owner: NativeRequestOwner,
    commands: mpsc::Sender<Command>,
    stop: CancelToken,
    _closed: CancelSubscription,
}
impl Drop for Selection {
    fn drop(&mut self) {
        self.stop.cancel();
        let _ = self.commands.send(Command::Close);
    }
}

#[derive(Default)]
pub(super) struct Selections(Arc<Mutex<HashMap<String, Selection>>>);

impl Selections {
    pub(super) fn list(
        &self,
        owner: &NativeRequestOwner,
        cancel: &CancelToken,
    ) -> Result<Vec<ScreenSource>, String> {
        capture::admitted(owner, cancel, || Ok(()))?;
        let (commands, receiver) = mpsc::channel();
        let stop = CancelToken::default();
        let original = owner.clone();
        let request = cancel.clone();
        let stopped = stop.clone();
        let wake = commands.clone();
        let (ready, result) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("sheets-wayland-selection".into())
            .spawn(move || {
                let wake_stop = stopped.on_cancel(move || {
                    let _ = wake.send(Command::Close);
                });
                let lost = original.loss_token();
                let stopped_source = stopped.clone();
                let source_loss = lost
                    .as_ref()
                    .map(|lost| lost.on_cancel(move || stopped_source.cancel()));
                let result = pollster::block_on(run_selection(
                    &original, &request, &stopped, receiver, &ready,
                ));
                if let Err(error) = result {
                    let _ = ready.send(Err(error));
                }
                drop((source_loss, wake_stop));
            })
            .map_err(|error| error.to_string())?;
        let preview = match result.recv() {
            Ok(Ok(frame)) => frame,
            Ok(Err(error)) => {
                stop.cancel();
                return Err(error);
            }
            Err(error) => {
                stop.cancel();
                return Err(error.to_string());
            }
        };
        let thumbnail = match capture::thumbnail(preview) {
            Ok(image) => image,
            Err(error) => {
                stop.cancel();
                return Err(error);
            }
        };
        let id = format!("window:portal-{}:0", jscompat::random_uuid());
        let weak = Arc::downgrade(&self.0);
        let closed_id = id.clone();
        let expected_stop = stop.clone();
        let closed = stop.on_cancel(move || {
            let gone = weak.upgrade().and_then(|selections| {
                selections.lock().ok().and_then(|mut selections| {
                    if selections
                        .get(&closed_id)
                        .is_some_and(|selection| selection.stop.same_token(&expected_stop))
                    {
                        selections.remove(&closed_id)
                    } else {
                        None
                    }
                })
            });
            drop(gone);
        });
        let selection = Selection {
            owner: owner.clone(),
            commands,
            stop,
            _closed: closed,
        };
        let source = ScreenSource {
            id: id.clone(),
            name: String::new(),
            thumbnail,
        };
        native_owner::with_owner_effect(owner, cancel, || {
            let mut selection = Some(selection);
            let inserted = {
                let mut selections = self
                    .0
                    .lock()
                    .map_err(|_| "The delegated capture state is poisoned")?;
                if selections.contains_key(&id) {
                    false
                } else {
                    selections.insert(id, selection.take().expect("one portal selection"));
                    true
                }
            };
            if !inserted {
                return Err("The delegated source identifier is already in use".into());
            }
            Ok(vec![source])
        })
        .ok_or_else(|| "The screen source request is no longer active".to_owned())?
    }

    pub(super) fn capture(
        &self,
        owner: &NativeRequestOwner,
        cancel: &CancelToken,
        id: &str,
        size: (u32, u32),
    ) -> Result<Option<ScreenImage>, String> {
        let original = self
            .0
            .lock()
            .map_err(|_| "The delegated capture state is poisoned")?
            .get(id)
            .map(|selection| selection.owner.clone());
        let Some(original) = original else {
            return capture::deliver(owner, cancel, None);
        };
        if !same_document(&original, owner) {
            return Err("The delegated source belongs to another document".into());
        }
        let selected = native_owner::with_paired_effect(&original, owner, cancel, cancel, || {
            self.0
                .lock()
                .map_err(|_| "The delegated capture state is poisoned")?
                .remove(id)
                .ok_or_else(|| "The delegated source was already consumed".to_owned())
        })
        .ok_or_else(|| "The delegated source is no longer authorized".to_owned())??;
        let (done, reply) = mpsc::sync_channel(1);
        let close = selected.stop.clone();
        let _cancel = cancel.on_cancel(move || close.cancel());
        selected
            .commands
            .send(Command::Capture {
                cancel: cancel.clone(),
                size,
                done,
            })
            .map_err(|_| "The selected portal session is closed")?;
        let image = reply
            .recv()
            .map_err(|_| "The selected portal capture did not complete")??;
        let image = capture::encoded_capture(image, size)?;
        capture::deliver(owner, cancel, image)
    }
}

fn same_document(original: &NativeRequestOwner, actual: &NativeRequestOwner) -> bool {
    original.view == actual.view
        && original
            .context
            .renderer()
            .zip(actual.context.renderer())
            .is_some_and(|(original, actual)| original.same_generation(actual))
}

async fn cancellable<T>(
    stop: &CancelToken,
    cancel: &CancelToken,
    future: impl Future<Output = Result<T, ashpd::Error>>,
) -> Result<T, String> {
    let (send, mut receive) = tokio::sync::oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(send)));
    let mut losses = Vec::with_capacity(2);
    for token in [stop, cancel] {
        let sender = sender.clone();
        losses.push(token.on_cancel(move || {
            if let Some(sender) = sender
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                let _ = sender.send(());
            }
        }));
    }
    let mut future = std::pin::pin!(future);
    poll_fn(|cx| {
        if stop.is_cancelled()
            || cancel.is_cancelled()
            || Pin::new(&mut receive).poll(cx).is_ready()
        {
            return Poll::Ready(Err("The portal capture request was cancelled".into()));
        }
        future
            .as_mut()
            .poll(cx)
            .map(|result| result.map_err(|error| error.to_string()))
    })
    .await
}

struct PortalSession {
    session: ashpd::desktop::Session<Screencast>,
    closed: bool,
}

impl PortalSession {
    async fn close(&mut self) -> Result<(), String> {
        self.closed = true;
        self.session
            .close()
            .await
            .map_err(|error| error.to_string())
    }
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
            if let Err(error) = pollster::block_on(self.session.close()) {
                eprintln!("[sheets] portal cleanup failed: {error}");
            }
        }
    }
}

async fn run_selection(
    owner: &NativeRequestOwner,
    cancel: &CancelToken,
    stop: &CancelToken,
    commands: mpsc::Receiver<Command>,
    ready: &FrameDone,
) -> Result<(), String> {
    capture::admitted(owner, cancel, || Ok(()))?;
    let proxy = cancellable(stop, cancel, Screencast::new()).await?;
    let mut session = PortalSession {
        session: proxy
            .create_session(Default::default())
            .await
            .map_err(|error| error.to_string())?,
        closed: false,
    };
    let result = async {
        capture::admitted(owner, cancel, || Ok(()))?;
        cancellable(
            stop,
            cancel,
            proxy.select_sources(
                &session.session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::HIDDEN)
                    .set_sources(SourceType::MONITOR | SourceType::WINDOW)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::DoNot),
            ),
        )
        .await?
        .response()
        .map_err(|error| error.to_string())?;
        capture::admitted(owner, cancel, || Ok(()))?;
        let response = cancellable(
            stop,
            cancel,
            proxy.start(&session.session, None, Default::default()),
        )
        .await?
        .response()
        .map_err(|error| error.to_string())?;
        let stream = response
            .streams()
            .first()
            .ok_or("The portal returned no selected source")?;
        let node = stream.pipe_wire_node_id();
        let fd = cancellable(
            stop,
            cancel,
            proxy.open_pipe_wire_remote(&session.session, Default::default()),
        )
        .await?;
        let preview = one_frame(owner, cancel, stop, node, fd, (320, 200))?;
        capture::admitted(owner, cancel, || Ok(()))?;
        ready
            .send(Ok(preview))
            .map_err(|_| "The source list caller was closed")?;
        match commands
            .recv()
            .map_err(|_| "The source selection was closed")?
        {
            Command::Close => Ok(None),
            Command::Capture { cancel, size, done } => {
                let result = async {
                    capture::admitted(owner, &cancel, || Ok(()))?;
                    let fd = cancellable(
                        stop,
                        &cancel,
                        proxy.open_pipe_wire_remote(&session.session, Default::default()),
                    )
                    .await?;
                    one_frame(owner, &cancel, stop, node, fd, size)
                }
                .await;
                Ok(Some((done, result)))
            }
        }
    }
    .await;
    let closed = session.close().await;
    match result {
        Ok(Some((done, frame))) => {
            let _ = done.send(frame.and_then(|frame| closed.map(|()| frame)));
            Ok(())
        }
        Ok(None) => closed,
        Err(error) => {
            let _ = closed;
            Err(error)
        }
    }
}

fn one_frame(
    owner: &NativeRequestOwner,
    cancel: &CancelToken,
    stop: &CancelToken,
    node: u32,
    fd: OwnedFd,
    size: (u32, u32),
) -> Result<RgbaImage, String> {
    capture::admitted(owner, cancel, || Ok(()))?;
    if stop.is_cancelled() {
        return Err("The selected portal session is closed".into());
    }
    pw::init();
    let mainloop = pw::main_loop::MainLoopBox::new(None).map_err(|error| error.to_string())?;
    let context =
        pw::context::ContextBox::new(mainloop.loop_(), None).map_err(|error| error.to_string())?;
    let core = context
        .connect_fd(fd, None)
        .map_err(|error| error.to_string())?;
    let stream = pw::stream::StreamBox::new(&core, "SeuOffice screen attachment", pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video", *pw::keys::MEDIA_CATEGORY => "Capture", *pw::keys::MEDIA_ROLE => "Screen",
    }).map_err(|error| error.to_string())?;
    let answer: Rc<RefCell<Option<Result<RgbaImage, String>>>> = Rc::default();
    let format_error = answer.clone();
    let state_error = answer.clone();
    let pixels = answer.clone();
    let _listener = stream
        .add_local_listener_with_user_data(VideoInfoRaw::default())
        .state_changed(move |_, _, _, state| {
            if let pw::stream::StreamState::Error(error) = state {
                *state_error.borrow_mut() = Some(Err(error.to_string()));
            }
        })
        .param_changed(move |_, format, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let result = format_utils::parse_format(param)
                .map_err(|error| error.to_string())
                .and_then(|(kind, subtype)| {
                    if kind != MediaType::Video || subtype != MediaSubtype::Raw {
                        return Err("The portal negotiated a non-raw video stream".into());
                    }
                    format.parse(param).map_err(|error| error.to_string())
                });
            if let Err(error) = result {
                *format_error.borrow_mut() = Some(Err(error));
            }
        })
        .process(move |stream, format| {
            if pixels.borrow().is_some() {
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let chunk = data.chunk();
            if chunk.size() == 0 {
                return;
            }
            let (offset, length, stride) = (
                chunk.offset() as usize,
                chunk.size() as usize,
                chunk.stride(),
            );
            if chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED) {
                *pixels.borrow_mut() = Some(Err("The portal returned a corrupted frame".into()));
                return;
            }
            let Some(bytes) = data.data() else {
                *pixels.borrow_mut() = Some(Err("The portal frame was not CPU-mapped".into()));
                return;
            };
            let size = format.size();
            *pixels.borrow_mut() = Some(decode_frame(
                bytes,
                offset,
                length,
                stride,
                size.width,
                size.height,
                format.format(),
            ));
        })
        .register()
        .map_err(|error| error.to_string())?;
    let object = spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::RGBA,
            VideoFormat::RGBA,
            VideoFormat::RGBx,
            VideoFormat::BGRA,
            VideoFormat::BGRx
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: size.0,
                height: size.1
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 4096,
                height: 4096
            }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 1, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 60, denom: 1 }
        )
    );
    let values =
        PodSerializer::serialize(Cursor::new(Vec::new()), &spa::pod::Value::Object(object))
            .map_err(|error| error.to_string())?
            .0
            .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or("Invalid PipeWire video negotiation pod")?];
    capture::admitted(owner, cancel, || {
        stream
            .connect(
                Direction::Input,
                Some(node),
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .map_err(|error| error.to_string())
    })?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if cancel.is_cancelled() || stop.is_cancelled() {
            return Err("The portal capture was cancelled".into());
        }
        capture::admitted(owner, cancel, || Ok(()))?;
        if let Some(result) = answer.borrow_mut().take() {
            return result;
        }
        if Instant::now() >= deadline {
            return Err("The selected portal source produced no video frame".into());
        }
        if mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(50)))
            < 0
        {
            return Err("The PipeWire loop failed".into());
        }
    }
}

fn decode_frame(
    bytes: &[u8],
    offset: usize,
    length: usize,
    stride: i32,
    width: u32,
    height: u32,
    format: VideoFormat,
) -> Result<RgbaImage, String> {
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err("Invalid portal video dimensions".into());
    }
    let row_bytes = (width as usize)
        .checked_mul(4)
        .ok_or("Portal video row overflow")?;
    let pitch = if stride == 0 {
        row_bytes
    } else {
        stride.unsigned_abs() as usize
    };
    if pitch < row_bytes {
        return Err("The portal video stride is shorter than its row".into());
    }
    let required = pitch
        .checked_mul(height as usize - 1)
        .and_then(|size| size.checked_add(row_bytes))
        .ok_or("Portal video size overflow")?;
    let end = offset
        .checked_add(length)
        .ok_or("Portal video offset overflow")?;
    let bytes = bytes
        .get(offset..end)
        .ok_or("The mapped portal frame is truncated")?;
    if required > bytes.len() {
        return Err("The mapped portal rows are truncated".into());
    }
    if !matches!(
        format,
        VideoFormat::RGBA | VideoFormat::RGBx | VideoFormat::BGRA | VideoFormat::BGRx
    ) {
        return Err("Unsupported negotiated portal video format".into());
    }
    let mut image = RgbaImage::new(width, height);
    for (y, target) in image.as_mut().chunks_exact_mut(row_bytes).enumerate() {
        let y = if stride < 0 {
            height as usize - 1 - y
        } else {
            y
        };
        let source = &bytes[y * pitch..y * pitch + row_bytes];
        for (source, target) in source.chunks_exact(4).zip(target.chunks_exact_mut(4)) {
            let (r, b) = if format == VideoFormat::BGRA || format == VideoFormat::BGRx {
                (source[2], source[0])
            } else {
                (source[0], source[2])
            };
            target.copy_from_slice(&[
                r,
                source[1],
                b,
                if format == VideoFormat::RGBx || format == VideoFormat::BGRx {
                    255
                } else {
                    source[3]
                },
            ]);
        }
    }
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mapped_bgra_rows_respect_offset_padding_and_alpha() {
        let bytes = [9, 9, 30, 20, 10, 255, 0, 0, 0, 0, 60, 50, 40, 128];
        let image = decode_frame(&bytes, 2, 12, 8, 1, 2, VideoFormat::BGRA).unwrap();
        assert_eq!(image.get_pixel(0, 0).0, [10, 20, 30, 255]);
        assert_eq!(image.get_pixel(0, 1).0, [40, 50, 60, 128]);
        assert!(decode_frame(&bytes[..12], 2, 12, 8, 1, 2, VideoFormat::BGRA).is_err());
    }
    #[test]
    fn inverted_bgrx_rows_do_not_expose_padding_as_alpha() {
        let image =
            decode_frame(&[3, 2, 1, 9, 6, 5, 4, 8], 0, 8, -4, 1, 2, VideoFormat::BGRx).unwrap();
        assert_eq!(image.get_pixel(0, 0).0, [4, 5, 6, 255]);
        assert_eq!(image.get_pixel(0, 1).0, [1, 2, 3, 255]);
    }
}
