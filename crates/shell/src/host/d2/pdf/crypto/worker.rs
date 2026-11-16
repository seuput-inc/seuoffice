//! One request-owned document-signing child. The protocol never accepts a digest.
pub(super) mod factory;
#[cfg(all(target_os = "linux", seuoffice_nss_bundle))]
mod nss_bridge;
#[cfg(all(target_os = "linux", seuoffice_nss_bundle))]
mod nss_bundle;
#[cfg(all(target_os = "linux", seuoffice_nss_bundle))]
mod nss_exchange;
pub(super) mod persistent;
pub(super) mod process;
mod state;
mod wire;

use super::session::{Admission, RetryEvidence, Session};
use serde_json::Value;
use shell_contract::pdf_sign_options::{SignOptions, VisibleSignature};
use shell_ipc_d2::pdf::editor::hosts::CertificateSource;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use wire::Kind;
use zeroize::Zeroizing;

type NonceFrames = ([u8; 32], Vec<Zeroizing<Vec<u8>>>);

const FIELDS: usize = 6;
const HEADER_LEN: usize = 32 + FIELDS * 8;

fn request_frame(input: &mut impl Read, limit: usize) -> Result<Vec<u8>, String> {
    let (kind, body) = wire::read_frame(input, limit)?;
    if kind != Kind::Request || body.len() != limit {
        return Err("invalid signing request frame".into());
    }
    Ok(body)
}
fn part<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], String> {
    if input.len() < 8 {
        return Err("invalid signing field length".into());
    }
    let length = usize::try_from(u64::from_be_bytes(
        input[..8].try_into().map_err(|_| "invalid signing field")?,
    ))
    .map_err(|_| "signing field overflow")?;
    *input = &input[8..];
    if length > input.len() {
        return Err("truncated signing field".into());
    }
    let (head, tail) = input.split_at(length);
    *input = tail;
    Ok(head)
}
fn append(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), String> {
    let length = u64::try_from(bytes.len()).map_err(|_| "signing field overflow")?;
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}
pub(super) fn encode_retry(evidence: &RetryEvidence) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    for value in evidence.byte_range {
        output.extend_from_slice(
            &u64::try_from(value)
                .map_err(|_| "signing range overflow")?
                .to_be_bytes(),
        );
    }
    for field in [
        &evidence.revision,
        &evidence.attributes_der,
        &evidence.signature,
        &evidence.cms_der,
    ] {
        append(&mut output, field)?;
    }
    Ok(output)
}
pub(super) fn decode_retry(mut input: &[u8]) -> Result<RetryEvidence, String> {
    if input.len() < 32 {
        return Err("invalid signing retry evidence".into());
    }

    let mut range = [0usize; 4];
    for (i, value) in range.iter_mut().enumerate() {
        *value = usize::try_from(u64::from_be_bytes(
            input[i * 8..i * 8 + 8]
                .try_into()
                .map_err(|_| "invalid signing range")?,
        ))
        .map_err(|_| "signing range overflow")?;
    }
    input = &input[32..];
    let revision = part(&mut input)?.to_vec();
    let attributes_der = part(&mut input)?.to_vec();
    let signature = part(&mut input)?.to_vec();
    let cms_der = part(&mut input)?.to_vec();
    if !input.is_empty() {
        return Err("trailing signing retry evidence".into());
    }
    Ok(RetryEvidence {
        revision,
        byte_range: range,
        attributes_der,
        signature,
        cms_der,
    })
}
/// Encode the immutable native request without placing secrets in arguments or environment.
pub(super) fn open_frames(
    source: &CertificateSource,
) -> Result<NonceFrames, String> {
    use main_core::pdf::system_certificates::SystemCertificateRef;
    let (kind, thumbprint, directory, nickname, p12, password) = match source {
        CertificateSource::P12 { p12, password } => (
            "p12",
            None,
            None,
            None,
            Zeroizing::new(p12.clone()),
            Zeroizing::new(password.as_bytes().to_vec()),
        ),
        CertificateSource::System {
            reference: SystemCertificateRef::Windows { thumbprint },
            store_password,
        } => (
            "windows-ref",
            Some(thumbprint.clone()),
            None,
            None,
            Zeroizing::new(Vec::new()),
            Zeroizing::new(store_password.as_deref().unwrap_or("").as_bytes().to_vec()),
        ),
        CertificateSource::System {
            reference: SystemCertificateRef::Nss { dir, nickname },
            store_password,
        } => (
            "nss",
            None,
            Some(dir.clone()),
            Some(nickname.clone()),
            Zeroizing::new(Vec::new()),
            Zeroizing::new(store_password.as_deref().unwrap_or("").as_bytes().to_vec()),
        ),
    };
    let metadata = serde_json::to_vec(&serde_json::json!({
        "op": "open", "kind": kind, "thumbprint": thumbprint,
        "dir": directory, "nickname": nickname,
    }))
    .map_err(|_| "invalid signing metadata")?;
    build_frames(
        [
            Zeroizing::new(metadata),
            Zeroizing::new(Vec::new()),
            p12,
            password,
            Zeroizing::new(Vec::new()),
            Zeroizing::new(Vec::new()),
        ],
        None,
    )
}

pub(super) fn verification_frames(
    document: &[u8],
    candidate: &[u8],
    options: &SignOptions,
    time: chrono::DateTime<chrono::Utc>,
    chain: &[Vec<u8>],
    retry: bool,
) -> Result<NonceFrames, String> {
    let visible = options.visible.as_ref().map(|v| {
        serde_json::json!({
            "page": v.page_index, "position": v.position, "width": v.width, "height": v.height,
        })
    });
    let metadata = serde_json::to_vec(&serde_json::json!({
        "op": if retry { "verify-retry" } else { "verify" }, "time": time.timestamp_millis(), "reason": options.reason,
        "location": options.location, "contact": options.contact_info,
        "certify": options.certify_level, "visible": visible,
    })).map_err(|_| "invalid signing metadata")?;
    let mut packed = Vec::new();
    for certificate in chain {
        append(&mut packed, certificate)?;
    }
    let png = options
        .visible
        .as_ref()
        .map_or_else(Vec::new, |visible| visible.png.clone());
    build_frames(
        [
            Zeroizing::new(metadata),
            Zeroizing::new(document.to_vec()),
            Zeroizing::new(candidate.to_vec()),
            Zeroizing::new(Vec::new()),
            Zeroizing::new(png),
            Zeroizing::new(packed),
        ],
        None,
    )
}

pub(super) fn sign_frames(
    document: &[u8],
    options: &SignOptions,
    time: chrono::DateTime<chrono::Utc>,
    chain: &[Vec<u8>],
    nonce: [u8; 32],
) -> Result<Vec<Zeroizing<Vec<u8>>>, String> {
    let visible = options.visible.as_ref().map(|v| {
        serde_json::json!({
            "page": v.page_index, "position": v.position, "width": v.width, "height": v.height,
        })
    });
    let metadata = serde_json::to_vec(&serde_json::json!({
        "op": "sign", "time": time.timestamp_millis(), "reason": options.reason,
        "location": options.location, "contact": options.contact_info,
        "certify": options.certify_level, "visible": visible,
    }))
    .map_err(|_| "invalid signing metadata")?;
    let mut packed = Vec::new();
    for certificate in chain {
        append(&mut packed, certificate)?;
    }
    let png = options
        .visible
        .as_ref()
        .map_or_else(Vec::new, |visible| visible.png.clone());
    let (_, frames) = build_frames(
        [
            Zeroizing::new(metadata),
            Zeroizing::new(document.to_vec()),
            Zeroizing::new(Vec::new()),
            Zeroizing::new(Vec::new()),
            Zeroizing::new(png),
            Zeroizing::new(packed),
        ],
        Some(nonce),
    )?;
    Ok(frames)
}

fn build_frames(
    fields: [Zeroizing<Vec<u8>>; FIELDS],
    nonce: Option<[u8; 32]>,
) -> Result<NonceFrames, String> {
    let nonce = match nonce {
        Some(nonce) => nonce,
        None => {
            let mut nonce = [0u8; 32];
            getrandom::fill(&mut nonce).map_err(|_| "signing nonce unavailable")?;
            nonce
        }
    };
    let mut header = Zeroizing::new(Vec::with_capacity(HEADER_LEN));
    header.extend_from_slice(&nonce);
    for field in &fields {
        header.extend_from_slice(
            &u64::try_from(field.len())
                .map_err(|_| "signing field overflow")?
                .to_be_bytes(),
        );
    }
    let mut frames = Vec::with_capacity(1 + FIELDS);
    frames.push(header);
    frames.extend(fields);
    Ok((nonce, frames))
}

struct Channel {
    input: std::io::Stdin,
    output: std::fs::File,
    nonce: [u8; 32],
}
impl Channel {
    fn send(&mut self, kind: Kind, bytes: &[u8]) -> Result<(), String> {
        let size = self
            .nonce
            .len()
            .checked_add(bytes.len())
            .ok_or("signing frame overflow")?;
        // Write a single framed message without copying document or CMS bytes.
        self.output
            .write_all(&[kind as u8])
            .and_then(|_| self.output.write_all(&(size as u64).to_be_bytes()))
            .and_then(|_| self.output.write_all(&self.nonce))
            .and_then(|_| self.output.write_all(bytes))
            .and_then(|_| self.output.flush())
            .map_err(|_| "signing worker output closed".to_owned())
    }
    fn query(&mut self, kind: Kind, evidence: &[u8]) -> Result<(), String> {
        self.send(kind, evidence)?;
        let (reply, bytes) = wire::read_frame(&mut self.input, self.nonce.len())?;
        if reply != Kind::Permit || bytes.as_slice() != self.nonce {
            return Err("signing request denied".into());
        }
        Ok(())
    }
}
struct Reader;
impl main_core::pdf::system_certificates::CertificateReader for Reader {
    fn read(&self, bytes: &[u8]) -> Option<main_core::pdf::system_certificates::CertSummary> {
        super::describe_certificate(bytes)
    }
}
#[derive(Debug, PartialEq, Eq)]
enum OpenFailure {
    Code(shell_ipc_d2::pdf::editor::hosts::CertificateOpenCode),
    Message(shell_ipc_d2::pdf::editor::hosts::CertificateOpenMessage),
}
impl From<String> for OpenFailure {
    fn from(error: String) -> Self {
        use shell_ipc_d2::pdf::editor::hosts::{CertificateOpenCode, CertificateOpenMessage};
        if matches!(
            error.as_str(),
            "signing frame allocation failed" | "signing frame length overflow"
        ) {
            return Self::Message(CertificateOpenMessage::ResourceExhausted);
        }
        CertificateOpenCode::parse(&error)
            .map(Self::Code)
            .unwrap_or(Self::Message(CertificateOpenMessage::WorkerUnavailable))
    }
}
impl From<&str> for OpenFailure {
    fn from(error: &str) -> Self {
        Self::from(error.to_owned())
    }
}
fn system_open_failure(error: main_core::pdf::system_certificates::Thrown) -> OpenFailure {
    use main_core::pdf::system_certificates::Thrown;
    use shell_ipc_d2::pdf::editor::hosts::CertificateOpenMessage;
    match error {
        Thrown::Store(error) => OpenFailure::from(error.code),
        Thrown::RegexLimit => OpenFailure::Message(CertificateOpenMessage::RegexLimit),
        Thrown::Node(_) | Thrown::JsonSyntax(_) => {
            OpenFailure::Message(CertificateOpenMessage::WorkerUnavailable)
        }
    }
}
fn send_open_failure(channel: &mut Channel, error: OpenFailure) -> Result<(), String> {
    use shell_ipc_d2::pdf::editor::hosts::CertificateOpenMessage;
    match error {
        OpenFailure::Code(code) => channel.send(Kind::Error, code.as_str().as_bytes()),
        OpenFailure::Message(message) => channel.send(
            Kind::OpenMessage,
            &[match message {
                CertificateOpenMessage::RegexLimit => 1,
                CertificateOpenMessage::WorkerUnavailable => 2,
                CertificateOpenMessage::Revoked => 3,
                CertificateOpenMessage::ResourceExhausted => 4,
            }],
        ),
    }
}
fn sign_failure(error: &str) -> &'static str {
    match error {
        "cert-password" => "cert-password",
        "cert-invalid" => "cert-invalid",
        "cert-no-key" => "cert-no-key",
        "cert-unsupported-key" => "cert-unsupported-key",
        "cert-not-exportable" => "cert-not-exportable",
        "cert-store-tool-missing" => "cert-store-tool-missing",
        "cert-store-locked" => "cert-store-locked",
        "cert-store-unsupported" => "cert-store-unsupported",
        "PDF is encrypted" => "pdf-encrypted",
        _ => "sign-failed",
    }
}
fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn decode_options(meta: &Value, png: Vec<u8>) -> Result<SignOptions, String> {
    let visible = match meta.get("visible") {
        None | Some(Value::Null) => None,
        Some(v) => Some(VisibleSignature {
            page_index: v
                .get("page")
                .and_then(Value::as_f64)
                .ok_or("invalid signing page")?,
            position: string(v, "position").ok_or("invalid signing position")?,
            width: v
                .get("width")
                .and_then(Value::as_f64)
                .ok_or("invalid signing width")?,
            height: v
                .get("height")
                .and_then(Value::as_f64)
                .ok_or("invalid signing height")?,
            png,
        }),
    };
    Ok(SignOptions {
        reason: string(meta, "reason"),
        location: string(meta, "location"),
        contact_info: string(meta, "contact"),
        certify_level: meta
            .get("certify")
            .and_then(Value::as_u64)
            .and_then(|v| u8::try_from(v).ok()),
        visible,
    })
}
fn chain_from_bytes(mut bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut chain = Vec::new();
    while !bytes.is_empty() {
        chain.push(part(&mut bytes)?.to_vec());
    }
    Ok(chain)
}

fn pack_opened(chain: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let mut packed_chain = Vec::new();
    let mut summaries = Vec::new();
    for certificate in chain {
        append(&mut packed_chain, certificate)?;
        let shell_contract::IpcValue::Object(fields) = super::certificate_info(certificate)? else {
            return Err("cert-invalid".into());
        };
        let mut object = serde_json::Map::new();
        for (key, value) in fields {
            let value = match value {
                shell_contract::IpcValue::String(value) => Value::String(value),
                shell_contract::IpcValue::Bool(value) => Value::Bool(value),
                _ => return Err("cert-invalid".into()),
            };
            object.insert(key, value);
        }
        summaries.push(Value::Object(object));
    }
    let summary_bytes = serde_json::to_vec(&summaries).map_err(|_| "cert-invalid")?;
    let mut result = Vec::new();
    append(&mut result, &packed_chain)?;
    append(&mut result, &summary_bytes)?;
    Ok(result)
}

pub(super) fn unpack_opened(
    mut input: &[u8],
) -> Result<(Vec<shell_contract::IpcValue>, Vec<Vec<u8>>), String> {
    let chain = chain_from_bytes(part(&mut input)?)?;
    let summaries: Value =
        serde_json::from_slice(part(&mut input)?).map_err(|_| "invalid certificate response")?;
    if !input.is_empty() {
        return Err("invalid certificate response".into());
    }
    let summaries = summaries
        .as_array()
        .ok_or("invalid certificate response")?
        .iter()
        .map(|value| {
            let object = value.as_object().ok_or("invalid certificate response")?;
            let fields = object
                .iter()
                .map(|(key, value)| {
                    let value = match value {
                        Value::String(value) => shell_contract::IpcValue::String(value.clone()),
                        Value::Bool(value) => shell_contract::IpcValue::Bool(*value),
                        _ => return Err("invalid certificate response"),
                    };
                    Ok((key.clone(), value))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(shell_contract::IpcValue::Object(fields))
        })
        .collect::<Result<Vec<_>, &str>>()
        .map_err(str::to_owned)?;
    if chain.is_empty() || chain.len() != summaries.len() {
        return Err("invalid certificate response".into());
    }
    Ok((summaries, chain))
}

/// Called only by the private pre-CEF worker dispatcher. Input is the inherited pipe.
fn read_bundle(
    input: &mut impl Read,
    expected: Option<[u8; 32]>,
) -> Result<NonceFrames, String> {
    let header = request_frame(input, HEADER_LEN)?;
    let nonce: [u8; 32] = header[..32]
        .try_into()
        .map_err(|_| "invalid signing request nonce")?;
    if expected.is_some_and(|expected| expected != nonce) {
        return Err("signing request nonce mismatch".into());
    }
    let mut sizes = [0usize; FIELDS];
    for (i, size) in sizes.iter_mut().enumerate() {
        *size = usize::try_from(u64::from_be_bytes(
            header[32 + i * 8..40 + i * 8]
                .try_into()
                .map_err(|_| "invalid signing request size")?,
        ))
        .map_err(|_| "signing request size overflow")?;
    }
    let mut fields = Vec::with_capacity(FIELDS);
    for size in sizes {
        fields.push(Zeroizing::new(request_frame(input, size)?));
    }
    Ok((nonce, fields))
}

enum Identity {
    P12(super::pkcs12::Identity),
    Windows(main_core::pdf::system_certificates::InPlaceSigner),
}

impl Identity {
    fn chain(&self) -> &[Vec<u8>] {
        match self {
            Self::P12(identity) => &identity.chain_der,
            Self::Windows(signer) => &signer.chain_der,
        }
    }
    fn sign(&self, digest: &[u8; 32]) -> Result<Vec<u8>, String> {
        match self {
            Self::P12(identity) => identity.sign_digest(digest),
            Self::Windows(signer) => {
                use main_core::pdf::system_certificates::{SystemStoreEnv, run_command};
                let reader = Reader;
                let now = || chrono::Utc::now().timestamp_millis() as f64;
                let env = SystemStoreEnv {
                    platform: main_core::pdf::font_locate::Platform::Win32,
                    home: "",
                    run: &run_command,
                    now: &now,
                    certificates: &reader,
                };
                signer.sign_digest(&env, digest).map_err(|error| error.code)
            }
        }
    }
}

/// Called only by the private pre-CEF worker dispatcher. Input is the inherited pipe.
pub(super) fn run_sign_worker(output: std::fs::File) -> Result<(), String> {
    let mut input = std::io::stdin();
    let (nonce, mut fields) = read_bundle(&mut input, None)?;
    let metadata: Value =
        serde_json::from_slice(&fields[0]).map_err(|_| "invalid signing request metadata")?;
    let operation = metadata
        .get("op")
        .and_then(Value::as_str)
        .ok_or("invalid signing operation")?;
    let channel = Arc::new(Mutex::new(Channel {
        input,
        output,
        nonce,
    }));
    if operation == "verify" || operation == "verify-retry" {
        let time = chrono::DateTime::from_timestamp_millis(
            metadata
                .get("time")
                .and_then(Value::as_i64)
                .ok_or("invalid signing time")?,
        )
        .ok_or("invalid signing time")?;
        let options = decode_options(&metadata, std::mem::take(&mut *fields[4]))?;
        let document = std::mem::take(&mut *fields[1]);
        channel
            .lock()
            .map_err(|_| "signing channel unavailable")?
            .query(Kind::Guard, &[])?;
        let chain = chain_from_bytes(&fields[5])?;
        let verified = if operation == "verify-retry" {
            decode_retry(&fields[2]).and_then(|evidence| {
                super::session::verify_first_retry(&document, &options, time, &chain, &evidence)
            })
        } else {
            super::sign_worker_verify::verify_signed_document(
                &document, &options, time, &chain, &fields[2],
            )
        };
        let mut channel = channel.lock().map_err(|_| "signing channel unavailable")?;
        channel.query(Kind::Guard, &[])?;
        return match verified {
            Ok(()) => channel.send(Kind::Verified, &[]),
            Err(_) => channel.send(Kind::Error, b"sign-failed"),
        };
    }
    if operation != "open" {
        return Err("invalid signing operation".into());
    }
    channel
        .lock()
        .map_err(|_| "signing channel unavailable")?
        .query(Kind::Guard, &[])?;
    let opened = (|| -> Result<Identity, OpenFailure> {
        match metadata.get("kind").and_then(Value::as_str) {
            Some("p12") => {
                let password = std::str::from_utf8(&fields[3]).map_err(|_| "cert-invalid")?;
                Ok(Identity::P12(super::pkcs12::open(
                    &fields[2], password, None,
                )?))
            }
            Some("windows-ref") => {
                use main_core::pdf::system_certificates::{
                    SystemCertificateRef, SystemIdentity, SystemStoreEnv, open_system_signer,
                    run_command,
                };
                let thumbprint = string(&metadata, "thumbprint").ok_or("cert-invalid")?;
                channel
                    .lock()
                    .map_err(|_| "signing channel unavailable")?
                    .query(Kind::AdmitFirst, &[])?;
                let reader = Reader;
                let now = || chrono::Utc::now().timestamp_millis() as f64;
                let env = SystemStoreEnv {
                    platform: main_core::pdf::font_locate::Platform::Win32,
                    home: "",
                    run: &run_command,
                    now: &now,
                    certificates: &reader,
                };
                let identity =
                    open_system_signer(&SystemCertificateRef::Windows { thumbprint }, None, &env)
                        .map_err(system_open_failure)?;
                match identity {
                    SystemIdentity::InPlace(signer) => {
                        signer.worker_identity().map_err(|e| e.code)?;
                        Ok(Identity::Windows(signer))
                    }
                    SystemIdentity::P12(_) => Err("cert-invalid".into()),
                }
            }
            #[cfg(all(target_os = "linux", seuoffice_nss_bundle))]
            Some("nss") => {
                let dir = string(&metadata, "dir").ok_or("cert-invalid")?;
                let nickname = string(&metadata, "nickname").ok_or("cert-invalid")?;
                let password = std::str::from_utf8(&fields[3]).map_err(|_| "cert-invalid")?;
                channel
                    .lock()
                    .map_err(|_| "signing channel unavailable")?
                    .query(Kind::AdmitFirst, &[])?;
                let bundle = nss_bundle::resolve_nss_bundle()?;
                let child = bundle.spawn_helper()?;
                let (p12, export_password) = nss_bridge::export(
                    child,
                    &dir,
                    &nickname,
                    password,
                    &mut *channel.lock().map_err(|_| "signing channel unavailable")?,
                )?;
                Ok(Identity::P12(super::pkcs12::open(
                    &p12,
                    &export_password,
                    None,
                )?))
            }
            #[cfg(not(all(target_os = "linux", seuoffice_nss_bundle)))]
            Some("nss") => Err("cert-store-unsupported".into()),
            _ => Err("cert-invalid".into()),
        }
    })();
    drop(fields);
    let identity = match opened {
        Ok(identity) => identity,
        Err(error) => {
            return send_open_failure(
                &mut *channel.lock().map_err(|_| "signing channel unavailable")?,
                error,
            );
        }
    };
    {
        let mut channel = channel.lock().map_err(|_| "signing channel unavailable")?;
        channel.query(Kind::Guard, &[])?;
        channel.send(Kind::Opened, &pack_opened(identity.chain())?)?;
    }
    let (_, mut fields) = {
        let mut channel = channel.lock().map_err(|_| "signing channel unavailable")?;
        read_bundle(&mut channel.input, Some(nonce))?
    };
    let metadata: Value =
        serde_json::from_slice(&fields[0]).map_err(|_| "invalid signing request metadata")?;
    if metadata.get("op").and_then(Value::as_str) != Some("sign")
        || !fields[2].is_empty()
        || !fields[3].is_empty()
    {
        return Err("invalid retained signing request".into());
    }
    let time = chrono::DateTime::from_timestamp_millis(
        metadata
            .get("time")
            .and_then(Value::as_i64)
            .ok_or("invalid signing time")?,
    )
    .ok_or("invalid signing time")?;
    let options = decode_options(&metadata, std::mem::take(&mut *fields[4]))?;
    let document = std::mem::take(&mut *fields[1]);
    let expected = chain_from_bytes(&fields[5])?;
    if identity.chain() != expected {
        return Err("signing identity changed".into());
    }
    let chain = identity.chain().to_vec();
    drop(fields);
    let guard_channel = channel.clone();
    let guard = Box::new(move || {
        guard_channel
            .lock()
            .map_err(|_| "signing channel unavailable".to_owned())?
            .query(Kind::Guard, &[])
    });
    let admit_channel = channel.clone();
    let admit = Box::new(move |event: Admission<'_>| {
        let (kind, evidence) = match event {
            Admission::First => (Kind::AdmitFirst, Vec::new()),
            Admission::Retry(evidence) => (Kind::AdmitRetry, encode_retry(evidence)?),
        };
        admit_channel
            .lock()
            .map_err(|_| "signing channel unavailable".to_owned())?
            .query(kind, &evidence)
    });
    let result = Session::new(
        document,
        options,
        time,
        chain,
        None,
        guard,
        admit,
        Box::new(move |digest| identity.sign(digest)),
    )
    .and_then(Session::sign);
    let mut channel = channel.lock().map_err(|_| "signing channel unavailable")?;
    match result {
        Ok(bytes) => channel.send(Kind::Signed, &bytes),
        Err(error) => channel.send(Kind::Error, sign_failure(&error).as_bytes()),
    }
}

#[cfg(test)]
mod worker_protocol_tests {
    use super::*;

    #[test]
    fn private_frames_preserve_fields_and_bind_the_nonce() {
        let fields = [
            Zeroizing::new(b"{\"op\":\"open\",\"kind\":\"p12\"}".to_vec()),
            Zeroizing::new(Vec::new()),
            Zeroizing::new(b"synthetic-p12".to_vec()),
            Zeroizing::new(b"synthetic-password".to_vec()),
            Zeroizing::new(Vec::new()),
            Zeroizing::new(Vec::new()),
        ];
        let nonce = [7; 32];
        let (_, frames) = build_frames(fields, Some(nonce)).unwrap();
        assert!(
            !frames[1]
                .windows(b"synthetic-password".len())
                .any(|part| part == b"synthetic-password")
        );
        let mut pipe = Vec::new();
        for frame in frames {
            wire::write_frame(&mut pipe, Kind::Request, &frame, frame.len()).unwrap();
        }
        let (_, decoded) = read_bundle(&mut pipe.as_slice(), Some(nonce)).unwrap();
        assert_eq!(decoded[2].as_slice(), b"synthetic-p12");
        assert_eq!(decoded[3].as_slice(), b"synthetic-password");
        assert!(read_bundle(&mut pipe.as_slice(), Some([8; 32])).is_err());
        assert!(read_bundle(&mut &pipe[..pipe.len() - 1], Some(nonce)).is_err());
    }
}

#[cfg(test)]
mod open_error_tests {
    use super::*;
    use main_core::pdf::system_certificates::{SystemStoreError, Thrown};
    use shell_ipc_d2::pdf::editor::hosts::{CertificateOpenCode, CertificateOpenMessage};

    #[test]
    fn source_store_and_regex_failures_remain_distinct() {
        assert_eq!(
            system_open_failure(Thrown::Store(SystemStoreError::new(
                "cert-store-tool-missing"
            ))),
            OpenFailure::Code(CertificateOpenCode::StoreToolMissing)
        );
        assert_eq!(
            system_open_failure(Thrown::Store(SystemStoreError::new("cert-not-exportable"))),
            OpenFailure::Code(CertificateOpenCode::NotExportable)
        );
        assert_eq!(
            system_open_failure(Thrown::RegexLimit),
            OpenFailure::Message(CertificateOpenMessage::RegexLimit)
        );
        assert_eq!(
            system_open_failure(Thrown::JsonSyntax("untrusted private output".into())),
            OpenFailure::Message(CertificateOpenMessage::WorkerUnavailable)
        );
    }
}
