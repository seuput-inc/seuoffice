//! Certificate (digital) signatures and the saved hand-drawn signatures.

use std::rc::Rc;

use jscompat::node_path::host::{dirname, join};
use jscompat::{JsonValue, buffer_from_base64};
use main_core::pdf::signature_store::{
    SavedSignature, add_signature, is_signature_data, load_signatures, remove_signature,
    save_signatures,
};
use main_core::pdf::system_certificates::{
    CertSummary, CertificateReader, SystemCertificateRef, SystemStoreEnv, Thrown,
    list_system_certificates, run_command,
};
use shell_contract::hosts::{FileDialog, FileFilter};
use shell_contract::pdf_sign_options::{SignOptions, VisibleSignature};
use shell_contract::runtime::PathName;
use shell_contract::{IpcValue, ViewId};

use super::dialogs::{Dlg, tm};
use super::editors::stem;
use super::hosts::{
    CertificateOpenCode, CertificateOpenError, CertificateSource, PdfEngineHost, SignFailure,
};
use super::save::is_same_redaction_copy_path;
use super::{
    CachedCertificate, CertEntry, CertificateCache, CertificateState, Editor, now_ms, read_file,
};
use crate::kit::{self, Entry, IntoIpc, as_num, as_str, error, field, obj};

/// `MAX_CERTIFICATE_FILE_BYTES`
const MAX_CERTIFICATE_FILE_BYTES: u64 = 4 * 1024 * 1024;
const SIGNATURE_POSITIONS: [&str; 5] = [
    "bottom-right",
    "bottom-left",
    "top-right",
    "top-left",
    "center",
];

/// `describeCertificate` through the engine, for the system store
struct Reader(PdfEngineHost);

impl CertificateReader for Reader {
    fn read(&self, input: &[u8]) -> Option<CertSummary> {
        self.0.describe_certificate(input)
    }
}

/// The system store env a worker builds around `run`
fn with_env<R>(
    engine: PdfEngineHost,
    platform: main_core::pdf::font_locate::Platform,
    home: String,
    f: impl FnOnce(&SystemStoreEnv<'_>) -> R,
) -> R {
    let reader = Reader(engine);
    let env = SystemStoreEnv {
        platform,
        home: &home,
        run: &run_command,
        now: &now_ms,
        certificates: &reader,
    };
    f(&env)
}

/// `err instanceof SystemStoreError ? err.code : fallback`
fn store_code(err: &Thrown, fallback: &str) -> String {
    match err {
        Thrown::Store(e) => e.code.clone(),
        _ => fallback.to_owned(),
    }
}

/// `readCertificateFile(path)`
fn read_certificate_file(path: &str) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(main_core::node_fs::namespaced(path))
        .map_err(|e| main_core::node_fs::node_error(&e, "stat", path).message)?;
    if meta.len() > MAX_CERTIFICATE_FILE_BYTES {
        return Err("certificate file too large".into());
    }
    read_file(path)
}

/// `text(value)` of `parseSignOptions`: trimmed, at most 500 UTF-16 units
fn sign_text(v: &IpcValue) -> Option<String> {
    let t = jscompat::trim(as_str(v)?);
    if t.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut units = 0;
    for c in t.chars() {
        units += c.len_utf16();
        if units > 500 {
            break;
        }
        out.push(c);
    }
    Some(out)
}

/// `parseSignOptions(request)`: `None` refuses the request
pub(super) fn parse_sign_options(request: &IpcValue) -> Option<SignOptions> {
    let certify_level = match field(request, "certifyLevel") {
        IpcValue::Undefined => None,
        IpcValue::Number(n) if [1.0, 2.0, 3.0].contains(n) => Some(*n as u8),
        _ => return None,
    };
    let mut out = SignOptions {
        reason: sign_text(field(request, "reason")),
        location: sign_text(field(request, "location")),
        contact_info: sign_text(field(request, "contactInfo")),
        certify_level,
        visible: None,
    };
    let v = field(request, "visible");
    if v != &IpcValue::Undefined {
        let num = |k| as_num(field(v, k)).filter(|n| n.is_finite());
        let page_index = num("pageIndex").filter(|n| n.fract() == 0.0 && *n >= 0.0)?;
        let position = as_str(field(v, "position")).filter(|p| SIGNATURE_POSITIONS.contains(p))?;
        let width = num("width").filter(|w| (40.0..=800.0).contains(w))?;
        let height = num("height").filter(|h| (20.0..=800.0).contains(h))?;
        let png =
            as_str(field(v, "png")).filter(|p| p.encode_utf16().count() <= 8 * 1024 * 1024)?;
        out.visible = Some(VisibleSignature {
            page_index,
            position: position.to_owned(),
            width,
            height,
            png: buffer_from_base64(png),
        });
    }
    Some(out)
}

fn signatures_ipc(list: &[SavedSignature]) -> IpcValue {
    IpcValue::Array(
        list.iter()
            .map(|s| {
                obj([
                    ("id", s.id.as_str().into_ipc()),
                    ("createdAt", s.created_at.into_ipc()),
                    ("data", kit::from_js_json(&s.data)),
                ])
            })
            .collect(),
    )
}

impl Editor {
    fn cert_entry(&self, view: ViewId, id: &IpcValue) -> Option<(String, CertEntry)> {
        let id = as_str(id)?;
        let entry = self.st.borrow().views.get(&view)?.certs.get(id).cloned()?;
        Some((id.to_owned(), entry))
    }

    /// `rememberCert(wcId, entry)`: the opaque id the renderer gets
    fn remember_cert(&self, view: ViewId, entry: CertEntry) -> String {
        let id = self.shell.random_uuid();
        self.with_view(view, |v| v.certs.insert(id.clone(), entry));
        id
    }

    fn release_cert(&self, view: ViewId, id: &str) {
        let cache = self
            .st
            .borrow()
            .views
            .get(&view)
            .and_then(|v| v.certs.get(id))
            .and_then(|entry| match entry {
                CertEntry::System { cache, .. } => Some(cache.clone()),
                CertEntry::File(_) => None,
            });
        if let Some(cache) = cache {
            let old = {
                let mut state = cache.borrow_mut();
                state.epoch = state.epoch.wrapping_add(1);
                std::mem::replace(&mut state.state, CertificateState::Invalid)
            };
            match old {
                CertificateState::Opening(token) | CertificateState::Busy(token) => token.cancel(),
                CertificateState::Ready(certificate) => certificate.session.revoke_token().cancel(),
                CertificateState::Empty | CertificateState::Invalid => {}
            }
            pump_certificate(&cache);
        }
    }

    fn signatures_path(&self) -> String {
        super::user_data_file(self, "pdf-signatures.json")
    }

    fn cert_material(
        self: &Rc<Self>,
        entry: CertEntry,
        password: String,
        store_password: Option<String>,
        authorization: (
            shell_contract::ipc::NativeRequestOwner,
            shell_contract::runtime::CancelToken,
        ),
        then: impl FnOnce(Result<Opened, Thrown>) + 'static,
    ) {
        let cache = match &entry {
            CertEntry::System { cache, .. } => Some(cache.clone()),
            CertEntry::File(_) => None,
        };
        if let Some(cache) = &cache {
            let state = {
                let mut slot = cache.borrow_mut();
                match &slot.state {
                    CertificateState::Opening(token) | CertificateState::Busy(token)
                        if token.is_cancelled() =>
                    {
                        slot.state = CertificateState::Invalid;
                        drop(slot);
                        then(Ok(Err(CertificateOpenError::Code(
                            CertificateOpenCode::Invalid,
                        ))));
                        pump_certificate(cache);
                        return;
                    }
                    CertificateState::Opening(_) | CertificateState::Busy(_) => {
                        let weak_editor = Rc::downgrade(self);
                        let weak_cache = Rc::downgrade(cache);
                        let CertEntry::System { reference, .. } = entry else {
                            unreachable!()
                        };
                        slot.waiting.push_back(Box::new(move || {
                            match (weak_editor.upgrade(), weak_cache.upgrade()) {
                                (Some(editor), Some(cache)) => editor.cert_material(
                                    CertEntry::System { reference, cache },
                                    password,
                                    store_password,
                                    authorization,
                                    then,
                                ),
                                _ => then(Err(certificate_expired())),
                            }
                        }));
                        return;
                    }
                    CertificateState::Invalid => {
                        drop(slot);
                        then(Ok(Err(CertificateOpenError::Code(
                            CertificateOpenCode::Invalid,
                        ))));
                        pump_certificate(cache);
                        return;
                    }
                    CertificateState::Empty => None,
                    CertificateState::Ready(_) => {
                        let CertificateState::Ready(certificate) =
                            std::mem::replace(&mut slot.state, CertificateState::Invalid)
                        else {
                            unreachable!()
                        };
                        let token = certificate.session.revoke_token();
                        if !certificate
                            .session
                            .is_current(&authorization.0, &authorization.1)
                        {
                            drop(slot);
                            token.cancel();
                            then(Ok(Err(CertificateOpenError::Code(
                                CertificateOpenCode::Invalid,
                            ))));
                            pump_certificate(cache);
                            return;
                        }
                        slot.state = CertificateState::Busy(token);
                        Some((certificate, slot.epoch))
                    }
                }
            };
            if let Some((certificate, epoch)) = state {
                then(Ok(Ok((certificate, Some(cache.clone()), epoch))));
                pump_certificate(cache);
                return;
            }
        }
        let token = shell_contract::runtime::CancelToken::default();
        let epoch = if let Some(cache) = &cache {
            let mut slot = cache.borrow_mut();
            slot.epoch = slot.epoch.wrapping_add(1);
            slot.state = CertificateState::Opening(token.clone());
            slot.epoch
        } else {
            0
        };
        let engine = self.engine.clone();
        let worker_authorization = authorization.clone();
        let worker_token = token.clone();
        let source = match entry {
            CertEntry::File(path) => CertificateInput::File(path, password),
            CertEntry::System { reference, .. } => {
                CertificateInput::System(reference, store_password)
            }
        };
        let opening_guard = CertificatePending {
            cache: cache.clone(),
            epoch,
            opening: true,
        };
        let ed = self.clone();
        kit::on_worker(
            &self.services,
            move || -> Result<OpenedCertificate, Thrown> {
                if !engine.sign_owner_current(&worker_authorization.0, &worker_authorization.1) {
                    return Err(certificate_expired());
                }
                let source = match source {
                    CertificateInput::File(path, password) => CertificateSource::P12 {
                        p12: read_certificate_file(&path).map_err(|message| {
                            Thrown::Node(main_core::node_fs::NodeError {
                                code: None,
                                message,
                            })
                        })?,
                        password,
                    },
                    CertificateInput::System(reference, store_password) => {
                        CertificateSource::System {
                            reference,
                            store_password,
                        }
                    }
                };
                Ok(engine
                    .open_signing_session(
                        &source,
                        &worker_authorization.0,
                        &worker_authorization.1,
                        &worker_token,
                    )
                    .map(|(session, summaries, chain)| CachedCertificate {
                        session,
                        summaries,
                        chain,
                    }))
            },
            move |result| {
                let _opening_guard = opening_guard;
                let current = ed
                    .engine
                    .sign_owner_current(&authorization.0, &authorization.1)
                    && !token.is_cancelled();
                let valid_epoch = cache
                    .as_ref()
                    .is_none_or(|cache| cache.borrow().epoch == epoch);
                if !current || !valid_epoch {
                    token.cancel();
                    if valid_epoch && let Some(cache) = &cache {
                        cache.borrow_mut().state = CertificateState::Invalid;
                    }
                    then(Err(certificate_expired()));
                    if let Some(cache) = &cache {
                        pump_certificate(cache);
                    }
                    return;
                }
                if let Some(cache) = &cache {
                    cache.borrow_mut().state = if matches!(&result, Ok(Ok(_))) {
                        CertificateState::Busy(token)
                    } else {
                        CertificateState::Empty
                    };
                }
                then(
                    result.map(|result| {
                        result.map(|certificate| (certificate, cache.clone(), epoch))
                    }),
                );
                if let Some(cache) = &cache {
                    pump_certificate(cache);
                }
            },
        );
    }
}

enum CertificateInput {
    File(String, String),
    System(SystemCertificateRef, Option<String>),
}

type OpenedCertificate = Result<CachedCertificate, CertificateOpenError>;
type Opened = Result<
    (
        CachedCertificate,
        Option<Rc<std::cell::RefCell<CertificateCache>>>,
        u64,
    ),
    CertificateOpenError,
>;

fn certificate_expired() -> Thrown {
    Thrown::Node(main_core::node_fs::NodeError {
        code: None,
        message: "pdf: signing request expired".into(),
    })
}

fn pump_certificate(cache: &Rc<std::cell::RefCell<CertificateCache>>) {
    let next = {
        let mut state = cache.borrow_mut();
        if matches!(
            state.state,
            CertificateState::Opening(_) | CertificateState::Busy(_)
        ) {
            None
        } else {
            state.waiting.pop_front()
        }
    };
    if let Some(next) = next {
        next();
    }
}

fn retain_certificate(
    cache: &Rc<std::cell::RefCell<CertificateCache>>,
    epoch: u64,
    certificate: CachedCertificate,
) {
    let mut state = cache.borrow_mut();
    if state.epoch == epoch && matches!(state.state, CertificateState::Busy(_)) {
        state.state = CertificateState::Ready(certificate);
    }
}

struct CertificatePending {
    cache: Option<Rc<std::cell::RefCell<CertificateCache>>>,
    epoch: u64,
    opening: bool,
}

impl Drop for CertificatePending {
    fn drop(&mut self) {
        let Some(cache) = &self.cache else { return };
        let token = {
            let mut state = cache.borrow_mut();
            if state.epoch != self.epoch {
                return;
            }
            let applies = if self.opening {
                matches!(state.state, CertificateState::Opening(_))
            } else {
                matches!(state.state, CertificateState::Busy(_))
            };
            if !applies {
                return;
            }
            match std::mem::replace(&mut state.state, CertificateState::Invalid) {
                CertificateState::Opening(token) | CertificateState::Busy(token) => Some(token),
                _ => None,
            }
        };
        if let Some(token) = token {
            token.cancel();
        }
        pump_certificate(cache);
    }
}

fn sign_failed(message: impl Into<String>) -> IpcValue {
    obj([
        ("ok", false.into_ipc()),
        ("error", "sign-failed".into_ipc()),
        ("message", message.into().into_ipc()),
    ])
}

fn sign_error(code: impl Into<String>) -> IpcValue {
    obj([("ok", false.into_ipc()), ("error", code.into().into_ipc())])
}

fn store_password(v: &IpcValue) -> Option<String> {
    as_str(v).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// One native read and its request provenance; the source path is not retained.
struct SigningSnapshot {
    bytes: Vec<u8>,
    sha256: String,
    owner: shell_contract::ipc::NativeRequestOwner,
    options: SignOptions,
}

impl SigningSnapshot {
    fn capture(
        path: &str,
        owner: shell_contract::ipc::NativeRequestOwner,
        options: SignOptions,
    ) -> Result<Self, String> {
        let bytes = read_file(path)?;
        let sha256 = main_core::markdown::fs_util::sha256_hex(&bytes);
        Ok(Self {
            bytes,
            sha256,
            owner,
            options,
        })
    }
}

fn sign(
    e: &Rc<Editor>,
    owner: shell_contract::ipc::NativeRequestOwner,
    cancel: shell_contract::runtime::CancelToken,
    request: IpcValue,
    reply: shell_contract::ipc::Responder,
) {
    let view = owner.view;
    if !e.engine.sign_owner_current(&owner, &cancel) {
        return reply.reply(Ok(sign_failed("pdf: signing request expired")));
    }
    let Some(path) = e.granted(view, field(&request, "path")) else {
        return reply.reply(Ok(sign_failed(super::NOT_GRANTED)));
    };
    if e.open_path(view).as_deref() != Some(path.as_str()) {
        return reply.reply(Ok(sign_failed(super::NOT_GRANTED)));
    }
    if e.st
        .borrow()
        .in_flight
        .get(&path)
        .is_some_and(|count| *count > 0)
    {
        return reply.reply(Ok(sign_failed("pdf: another save is in progress")));
    }
    let entry = e.cert_entry(view, field(&request, "certId"));
    let (Some((id, entry)), Some(password)) = (
        entry,
        as_str(field(&request, "password")).map(str::to_owned),
    ) else {
        return reply.reply(Ok(sign_error("cert-invalid")));
    };
    let Some(options) = parse_sign_options(&request) else {
        return reply.reply(Ok(sign_failed("pdf: invalid sign request")));
    };
    let busy =
        e.st.borrow()
            .views
            .get(&view)
            .is_some_and(|v| v.redaction_flow);
    if busy || e.save_as_waiting(view) {
        return reply.reply(Ok(sign_failed("pdf: another save is in progress")));
    }
    // the signature covers the file on disk; edits still in the renderer would be left out
    if e.st.borrow().views.get(&view).is_some_and(|v| v.dirty) {
        return reply.reply(Ok(sign_failed("pdf: save pending edits before signing")));
    }
    let operation = kit::unique();
    e.with_view(view, |v| {
        v.redaction_flow = true;
        v.sign_operation = Some(operation);
    });
    e.set_save_as_in_flight(view, true);
    let me = Rc::downgrade(e);
    let finish_id = id.clone();
    let finish_owner = owner.clone();
    let finish_cancel = cancel.clone();
    let finish = move |answer: IpcValue| {
        if let Some(ed) = me.upgrade() {
            let owns_flow = {
                let mut state = ed.st.borrow_mut();
                match state.views.get_mut(&view) {
                    Some(slot) if slot.sign_operation == Some(operation) => {
                        slot.sign_operation = None;
                        slot.redaction_flow = false;
                        true
                    }
                    _ => false,
                }
            };
            if owns_flow {
                if ed.engine.sign_owner_current(&finish_owner, &finish_cancel) {
                    ed.release_cert(view, &finish_id);
                }
                ed.set_save_as_in_flight(view, false);
            }
        }
        reply.reply(Ok(answer))
    };
    let store_password = store_password(field(&request, "storePassword"));
    let snapshot_path = path.clone();
    let snapshot_owner = owner.clone();
    let snapshot_cancel = cancel.clone();
    let snapshot_engine = e.engine.clone();
    let ed = e.clone();
    kit::on_worker(
        &e.services,
        move || {
            if !snapshot_engine.sign_owner_current(&snapshot_owner, &snapshot_cancel) {
                return Err("pdf: signing request expired".to_owned());
            }
            let snapshot = SigningSnapshot::capture(&snapshot_path, snapshot_owner, options)?;
            if !snapshot_engine.sign_owner_current(&snapshot.owner, &snapshot_cancel) {
                return Err("pdf: signing request expired".to_owned());
            }
            Ok(snapshot)
        },
        move |snapshot| {
            let snapshot = match snapshot {
                Ok(snapshot) => snapshot,
                Err(error) => return finish(sign_failed(error)),
            };
            if !ed.engine.sign_owner_current(&snapshot.owner, &cancel) {
                return finish(sign_failed("pdf: signing request expired"));
            }
            let owner = snapshot.owner.clone();
            let cert_ed = ed.clone();
            ed.cert_material(
                entry,
                password,
                store_password,
                (owner.clone(), cancel.clone()),
                move |material| {
                    let ed = cert_ed;
                    let (mut certificate, cache, epoch) = match material {
                        Ok(Ok(opened)) => opened,
                        Ok(Err(CertificateOpenError::Code(code))) => {
                            return finish(sign_error(code.as_str()));
                        }
                        Ok(Err(CertificateOpenError::Message(message))) => {
                            return finish(sign_failed(message.as_str()));
                        }
                        Err(Thrown::Store(err)) => return finish(sign_error(err.code)),
                        Err(err) => return finish(sign_failed(thrown_message(&err))),
                    };
                    let pending_guard = CertificatePending {
                        cache,
                        epoch,
                        opening: false,
                    };
                    if !ed.engine.sign_owner_current(&owner, &cancel) {
                        return finish(sign_failed("pdf: signing request expired"));
                    }
                    if certificate.chain.is_empty() {
                        return finish(sign_failed("pdf: missing signing certificate chain"));
                    }
                    let picked = ed.dialogs.save(
                        ed.contents.window_of(view),
                        &FileDialog {
                            title: Some(tm(&ed.lang(), Dlg::SignedCopy).into()),
                            default_path: Some(
                                join(&[&dirname(&path), &format!("{}-signed.pdf", stem(&path))])
                                    .into(),
                            ),
                            filters: vec![FileFilter {
                                name: tm(&ed.lang(), Dlg::FilterPdf).into(),
                                extensions: vec!["pdf".into()],
                            }],
                            ..Default::default()
                        },
                    );
                    let cancelled =
                        || obj([("ok", false.into_ipc()), ("cancelled", true.into_ipc())]);
                    let Some(target) = picked
                        .map(|p| p.to_string_lossy().into_owned())
                        .filter(|p| !p.is_empty())
                    else {
                        return finish(cancelled());
                    };
                    // never overwrite the source
                    if is_same_redaction_copy_path(&path, &target) {
                        return finish(cancelled());
                    }
                    let engine = ed.engine.clone();
                    let ed2 = ed.clone();
                    let written = target.clone();
                    let source = path.clone();
                    let worker_owner = owner.clone();
                    let worker_cancel = cancel.clone();
                    kit::on_worker(
                        &ed.services,
                        move || -> Result<(), SignFailure> {
                            if !engine.sign_owner_current(&worker_owner, &worker_cancel) {
                                return Err(SignFailure::Message(
                                    "pdf: signing request expired".into(),
                                ));
                            }
                            if main_core::markdown::fs_util::sha256_hex(&snapshot.bytes)
                                != snapshot.sha256
                            {
                                return Err(SignFailure::Message(
                                    "pdf: signing snapshot changed".into(),
                                ));
                            }
                            let signed = certificate.session.sign(
                                &snapshot.bytes,
                                &snapshot.options,
                                &certificate.chain,
                                &worker_owner,
                                &worker_cancel,
                            )?;
                            if !engine.sign_owner_current(&worker_owner, &worker_cancel) {
                                return Err(SignFailure::Message(
                                    "pdf: signing request expired".into(),
                                ));
                            }
                            engine
                                .write_signed_pdf(&written, &signed, &worker_owner, &worker_cancel)
                                .map_err(SignFailure::Message)?;
                            Ok(())
                        },
                        move |result| {
                            let _pending_guard = pending_guard;
                            match result {
                                Ok(()) => {
                                    if !ed2.engine.sign_owner_current(&owner, &cancel) {
                                        return finish(sign_failed("pdf: signing request expired"));
                                    }
                                    let ed = ed2;
                                    // Finish this request's state before revoking its old slot.
                                    ed.release_cert(view, &id);
                                    ed.with_view(view, |v| v.redaction_flow = false);
                                    ed.set_save_as_in_flight(view, false);
                                    ed.grant_only(view, &target);
                                    ed.replace_document(view, Some(target.clone()));
                                    // the signed file starts clean: no copy of the unsigned document follows
                                    ed.st.borrow_mut().unsaved_untitled_copy.remove(&source);
                                    ed.rec
                                        .borrow_mut()
                                        .moved_to_new_file(view, &source, &target);
                                    if let Err(err) = ed.shell.redaction_saved(view, &target) {
                                        eprintln!("[pdf] signed-copy hook failed: {err}");
                                    }
                                    finish(obj([
                                        ("ok", true.into_ipc()),
                                        ("path", target.into_ipc()),
                                    ]))
                                }
                                Err(SignFailure::Code(code)) => finish(sign_error(code)),
                                Err(SignFailure::Message(m)) => finish(sign_failed(m)),
                            }
                        },
                    );
                },
            );
        },
    );
}

fn thrown_message(err: &Thrown) -> String {
    match err {
        Thrown::Store(e) => e.code.clone(),
        Thrown::Node(e) => e.message.clone(),
        Thrown::JsonSyntax(m) => m.clone(),
        Thrown::RegexLimit => "Maximum call stack size exceeded".into(),
    }
}

pub(super) fn entries(ed: &Rc<Editor>) -> Vec<Entry> {
    let mut all = Vec::new();

    let e = ed.clone();
    all.push(kit::invoke_later(
        "pdf:list-digital-signatures",
        move |cx, args, reply| {
            let view = cx.sender.view;
            let Some(path) = e.granted(view, args.get(0)) else {
                return reply.reply(Ok(IpcValue::Array(Vec::new())));
            };
            let bytes = e.document_bytes(view, &path);
            let engine = e.engine.clone();
            kit::on_worker(
                &e.services,
                move || engine.read_pdf_signatures(&bytes()?),
                move |r| reply.reply(Ok(r.unwrap_or(IpcValue::Array(Vec::new())))),
            );
        },
    ));

    // the renderer only ever sees an opaque id; the path stays here
    let e = ed.clone();
    all.push(kit::invoke("pdf:pick-certificate", move |cx, _| {
        let view = cx.sender.view;
        let lang = e.lang();
        let picked = e.dialogs.open(
            e.contents.window_of(view),
            &FileDialog {
                title: Some(tm(&lang, Dlg::PickCertificate).into()),
                properties: vec!["openFile".into()],
                filters: vec![FileFilter {
                    name: tm(&lang, Dlg::FilterCertificate).into(),
                    extensions: vec!["p12".into(), "pfx".into()],
                }],
                ..Default::default()
            },
        );
        let Some(file) = picked.and_then(|p| p.into_iter().next()) else {
            return Ok(IpcValue::Null);
        };
        let file = file.to_string_lossy().into_owned();
        let name = jscompat::node_path::host::basename(&file, None);
        let cert_id = e.remember_cert(view, CertEntry::File(file));
        Ok(obj([
            ("certId", cert_id.into_ipc()),
            ("fileName", name.into_ipc()),
        ]))
    }));

    let e = ed.clone();
    all.push(kit::invoke_later(
        "pdf:list-system-certificates",
        move |cx, args, reply| {
            let view = cx.sender.view;
            let password = store_password(args.get(0));
            let engine = e.engine.clone();
            let platform = e.shell.platform();
            let home = e.path(PathName::Home);
            let ed = e.clone();
            kit::on_worker(
                &e.services,
                move || {
                    with_env(engine, platform, home, |env| {
                        list_system_certificates(password.as_deref(), env)
                    })
                },
                move |listing| {
                    let listing = match listing {
                        Ok(l) => l,
                        Err(err) => return reply.reply(Err(error(thrown_message(&err)))),
                    };
                    // a fresh listing replaces the previous one
                    ed.with_view(view, |v| {
                        v.certs
                            .retain(|_, c| !matches!(c, CertEntry::System { .. }))
                    });
                    let certs = listing
                        .certs
                        .into_iter()
                        .map(|c| {
                            let reference: SystemCertificateRef = c.reference;
                            let id = ed.remember_cert(
                                view,
                                CertEntry::System {
                                    reference,
                                    cache: Rc::new(std::cell::RefCell::new(
                                        CertificateCache::default(),
                                    )),
                                },
                            );
                            obj([
                                ("certId", id.into_ipc()),
                                ("label", c.label.into_ipc()),
                                ("commonName", c.common_name.into_ipc()),
                                ("issuerCommonName", c.issuer_common_name.into_ipc()),
                                ("validTo", c.valid_to.into_ipc()),
                                ("expired", c.expired.into_ipc()),
                            ])
                        })
                        .collect();
                    reply.reply(Ok(obj([
                        (
                            "issues",
                            listing
                                .issues
                                .iter()
                                .map(|i| i.as_str())
                                .collect::<Vec<_>>()
                                .into_ipc(),
                        ),
                        ("certs", IpcValue::Array(certs)),
                    ])))
                },
            );
        },
    ));

    let e = ed.clone();
    all.push(kit::send("pdf:release-certificate", move |cx, args| {
        if let Some(id) = args.str(0) {
            e.release_cert(cx.sender.view, id);
        }
        Ok(())
    }));

    let e = ed.clone();
    all.push(kit::invoke_later(
        "pdf:inspect-certificate",
        move |cx, args, reply| {
            let view = cx.sender.view;
            let Some(context) = cx.native.clone() else {
                return reply.reply(Ok(sign_error("cert-invalid")));
            };
            let Some(owner) = e.bind_pdf_owner(view, context) else {
                return reply.reply(Ok(sign_error("cert-invalid")));
            };
            if !e.engine.sign_owner_current(&owner, &cx.cancel) {
                return reply.reply(Ok(sign_error("cert-invalid")));
            }
            let entry = e.cert_entry(view, args.get(0));
            let (Some((_id, entry)), Some(password)) = (entry, args.str(1).map(str::to_owned))
            else {
                return reply.reply(Ok(sign_error("cert-invalid")));
            };
            let inspect_cancel = cx.cancel.clone();
            e.cert_material(
                entry,
                password,
                store_password(args.get(2)),
                (owner.clone(), cx.cancel.clone()),
                move |material| {
                    let answer = match material {
                        Err(err) => sign_error(store_code(&err, "cert-invalid")),
                        Ok(opened) => match opened {
                            Ok((certificate, cache, epoch)) => {
                                if !certificate.session.is_current(&owner, &inspect_cancel) {
                                    certificate.session.revoke_token().cancel();
                                    if let Some(cache) = cache {
                                        let mut state = cache.borrow_mut();
                                        if state.epoch == epoch {
                                            state.state = CertificateState::Invalid;
                                        }
                                    }
                                    return reply.reply(Ok(sign_error("cert-invalid")));
                                }
                                let info = certificate.summaries.clone();
                                if let Some(cache) = cache {
                                    retain_certificate(&cache, epoch, certificate);
                                }
                                obj([
                                    ("ok", true.into_ipc()),
                                    (
                                        "signer",
                                        info.first().cloned().unwrap_or(IpcValue::Undefined),
                                    ),
                                    ("chain", IpcValue::Array(info)),
                                ])
                            }
                            Err(CertificateOpenError::Code(code)) => sign_error(code.as_str()),
                            Err(CertificateOpenError::Message(_)) => sign_error("cert-invalid"),
                        },
                    };
                    reply.reply(Ok(answer))
                },
            );
        },
    ));

    let e = ed.clone();
    all.push(kit::invoke_later(
        "pdf:sign-with-certificate",
        move |cx, mut args, reply| {
            let Some(context) = cx.native.clone() else {
                return reply.reply(Ok(sign_failed("pdf: signing request is not authenticated")));
            };
            let Some(owner) = e.bind_sign_owner(cx.sender.view, context) else {
                return reply.reply(Ok(sign_failed("pdf: signing request is not authenticated")));
            };
            sign(&e, owner, cx.cancel.clone(), args.take(0), reply)
        },
    ));

    let e = ed.clone();
    all.push(kit::invoke("pdf:list-signatures", move |_, _| {
        Ok(signatures_ipc(&load_signatures(&e.signatures_path())))
    }));

    let e = ed.clone();
    all.push(kit::invoke("pdf:add-signature", move |_, args| {
        let path = e.signatures_path();
        let list = load_signatures(&path);
        let data = kit::to_js_json(args.get(0)).unwrap_or(JsonValue::Null);
        if !is_signature_data(&data) {
            return Ok(signatures_ipc(&list));
        }
        let next = add_signature(&list, &data, now_ms());
        save_signatures(&path, &next, std::process::id()).map_err(|e| error(e.message))?;
        Ok(signatures_ipc(&next))
    }));

    let e = ed.clone();
    all.push(kit::invoke("pdf:remove-signature", move |_, args| {
        let path = e.signatures_path();
        let list = load_signatures(&path);
        let Some(id) = args.str(0) else {
            return Ok(signatures_ipc(&list));
        };
        let next = remove_signature(&list, id);
        if next.len() != list.len() {
            save_signatures(&path, &next, std::process::id()).map_err(|e| error(e.message))?;
        }
        Ok(signatures_ipc(&next))
    }));

    all
}

#[cfg(test)]
mod tests {
    use super::super::editors::PdfEditors;
    use super::*;
    use serde_json::Value;
    use shell_contract::Surface;

    #[test]
    fn bounded_worker_failure_keeps_inspect_and_sign_reply_shapes() {
        let (mut rt, _) = super::super::tests::rt_with(
            serde_json::json!({"openCertificateMessage":"worker-unavailable"}),
        );
        let view = rt.view(1, Surface::Pdf);
        let editors = rt.provided().all::<PdfEditors>().pop().unwrap();
        let document = rt.dir.join("source.pdf");
        let certificate = rt.dir.join("synthetic.p12");
        std::fs::write(&document, b"%PDF synthetic").unwrap();
        std::fs::write(&certificate, b"synthetic").unwrap();
        let document = document.to_string_lossy().into_owned();
        editors.open_view(view.view, Some(&document));
        let id = editors.0.remember_cert(
            view.view,
            CertEntry::File(certificate.to_string_lossy().into_owned()),
        );
        let inspect = rt
            .invoke_now(
                "pdf:inspect-certificate",
                view,
                vec![IpcValue::String(id.clone()), IpcValue::String("pw".into())],
            )
            .unwrap();
        assert_eq!(
            field(&inspect, "error"),
            &IpcValue::String("cert-invalid".into())
        );
        let sign = rt
            .invoke_now(
                "pdf:sign-with-certificate",
                view,
                vec![obj([
                    ("path", IpcValue::String(document)),
                    ("certId", IpcValue::String(id)),
                    ("password", IpcValue::String("pw".into())),
                ])],
            )
            .unwrap();
        assert_eq!(
            field(&sign, "error"),
            &IpcValue::String("sign-failed".into())
        );
        assert_eq!(
            field(&sign, "message"),
            &IpcValue::String("certificate worker unavailable".into())
        );
    }

    #[test]
    fn system_inspection_reuses_child_and_loss_does_not_reopen() {
        let (mut rt, _) = super::super::tests::rt_with(Value::Null);
        let view = rt.view(1, Surface::Pdf);
        let editors = rt.provided().all::<PdfEditors>().pop().unwrap();
        editors.open_view(view.view, None);
        let cache = Rc::new(std::cell::RefCell::new(CertificateCache::default()));
        let id = editors.0.remember_cert(
            view.view,
            CertEntry::System {
                reference: SystemCertificateRef::Windows {
                    thumbprint: "synthetic".into(),
                },
                cache: cache.clone(),
            },
        );
        let args = || vec![IpcValue::String(id.clone()), IpcValue::String("pw".into())];
        let first = rt
            .invoke_now("pdf:inspect-certificate", view, args())
            .unwrap();
        assert_eq!(field(&first, "ok"), &IpcValue::Bool(true));
        let token = match &cache.borrow().state {
            CertificateState::Ready(certificate) => certificate.session.revoke_token(),
            _ => panic!("inspection did not retain its session"),
        };
        let second = rt
            .invoke_now("pdf:inspect-certificate", view, args())
            .unwrap();
        assert_eq!(field(&second, "ok"), &IpcValue::Bool(true));
        token.cancel();
        let after_loss = rt
            .invoke_now("pdf:inspect-certificate", view, args())
            .unwrap();
        assert_eq!(
            field(&after_loss, "error"),
            &IpcValue::String("cert-invalid".into())
        );
    }

    #[test]
    fn cached_system_identity_is_not_reused_by_replacement_renderer() {
        let (mut rt, _) = super::super::tests::rt_with(Value::Null);
        let view = rt.view(1, Surface::Pdf);
        let editors = rt.provided().all::<PdfEditors>().pop().unwrap();
        editors.open_view(view.view, None);
        let cache = Rc::new(std::cell::RefCell::new(CertificateCache::default()));
        let id = editors.0.remember_cert(
            view.view,
            CertEntry::System {
                reference: SystemCertificateRef::Windows {
                    thumbprint: "synthetic".into(),
                },
                cache: cache.clone(),
            },
        );
        let args = || vec![IpcValue::String(id.clone()), IpcValue::String("pw".into())];
        let first = rt
            .invoke_now("pdf:inspect-certificate", view, args())
            .unwrap();
        assert_eq!(field(&first, "ok"), &IpcValue::Bool(true));
        let token = match &cache.borrow().state {
            CertificateState::Ready(certificate) => certificate.session.revoke_token(),
            _ => panic!("inspection did not retain its session"),
        };
        rt.replace_renderer(view.view);
        let second = rt
            .invoke_now("pdf:inspect-certificate", view, args())
            .unwrap();
        assert_eq!(
            field(&second, "error"),
            &IpcValue::String("cert-invalid".into())
        );
        assert!(token.is_cancelled());
    }

    #[test]
    fn release_during_open_revokes_worker_and_settles_queued_inspection() {
        use super::super::fakes::{engine, shell};
        use super::super::hosts::{PdfEngineHost, PdfShellHost};
        use crate::testing::Rt;
        use main_core::packages::electron_utils::recovery_store::SharedRecoveryState;
        use shell_contract::runtime::Workers;
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Deferred(Mutex<Vec<Box<dyn FnOnce() + Send>>>);
        impl Workers for Deferred {
            fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
                self.0.lock().unwrap().push(job);
            }
        }
        let mut rt = Rt::new(|hosts| {
            hosts.insert::<PdfEngineHost>(engine(&Value::Null));
            hosts.insert::<PdfShellHost>(shell(&Value::Null));
            hosts.insert(Arc::new(Mutex::new(SharedRecoveryState::new("test"))));
            crate::pdf::tools::fakes(hosts, &Value::Null);
        });
        let workers = Arc::new(Deferred::default());
        rt.services.workers = workers.clone();
        rt.register(crate::pdf::entries).unwrap();
        let view = rt.view(1, Surface::Pdf);
        let editors = rt.provided().all::<PdfEditors>().pop().unwrap();
        editors.open_view(view.view, None);
        let cache = Rc::new(std::cell::RefCell::new(CertificateCache::default()));
        let id = editors.0.remember_cert(
            view.view,
            CertEntry::System {
                reference: SystemCertificateRef::Windows {
                    thumbprint: "synthetic".into(),
                },
                cache: cache.clone(),
            },
        );
        let args = || vec![IpcValue::String(id.clone()), IpcValue::String("pw".into())];
        let first = rt.invoke("pdf:inspect-certificate", view, args());
        let second = rt.invoke("pdf:inspect-certificate", view, args());
        let token = match &cache.borrow().state {
            CertificateState::Opening(token) => token.clone(),
            _ => panic!("first inspection did not start opening"),
        };
        rt.send("pdf:release-certificate", view, vec![IpcValue::String(id)]);
        assert!(token.is_cancelled());
        let answer = second.take().unwrap().unwrap();
        assert_eq!(
            field(&answer, "error"),
            &IpcValue::String("cert-invalid".into())
        );
        let worker = workers.0.lock().unwrap().remove(0);
        worker();
        rt.drain();
        assert!(first.take().is_some());
        assert!(matches!(cache.borrow().state, CertificateState::Invalid));
    }
}
