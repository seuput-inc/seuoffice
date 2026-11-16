use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, Utc};
use shell_contract::pdf_sign_options::SignOptions;

use super::{DigestAlgorithm, cms, pdf_lib, verify_signature};

/// Bytes returned by the isolated first attempt. None of these fields supplies
/// authority: the parent verifies them against its own immutable request.
#[derive(Clone)]
pub(super) struct RetryEvidence {
    pub(super) revision: Vec<u8>,
    pub(super) byte_range: [usize; 4],
    pub(super) attributes_der: Vec<u8>,
    pub(super) signature: Vec<u8>,
    pub(super) cms_der: Vec<u8>,
}

pub(super) enum Admission<'a> {
    First,
    Retry(&'a RetryEvidence),
}

enum State {
    ReadyFirst,
    Terminal,
}

type Admit = Box<dyn for<'a> Fn(Admission<'a>) -> Result<(), String> + Send>;
type Signer = Box<dyn FnMut(&[u8; 32]) -> Result<Vec<u8>, String> + Send>;

struct Request {
    original: Vec<u8>,
    options: SignOptions,
    pdf_time: String,
    der_time: Vec<u8>,
    chain: Vec<Vec<u8>>,
    deadline: Option<Instant>,
    guard: Box<dyn Fn() -> Result<(), String> + Send>,
    admit: Admit,
    signer: Signer,
}

fn authorize(request: &Request) -> Result<(), String> {
    if request
        .deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        return Err("signing session expired".into());
    }
    (request.guard)()
}

fn signing_times(time: DateTime<Utc>) -> Result<(String, Vec<u8>), String> {
    let pdf_time = time.format("D:%Y%m%d%H%M%SZ").to_string();
    let (tag, formatted) = if (1950..2050).contains(&time.year()) {
        (0x17, time.format("%y%m%d%H%M%SZ").to_string())
    } else {
        (0x18, time.format("%Y%m%d%H%M%SZ").to_string())
    };
    let mut der_time = vec![
        tag,
        u8::try_from(formatted.len()).map_err(|_| "invalid signing time")?,
    ];
    der_time.extend_from_slice(formatted.as_bytes());
    Ok((pdf_time, der_time))
}

/// Parent-side first-attempt verification. Chain, input, options and time must
/// be captured by native code independently of the worker evidence.
pub(super) fn verify_first_retry(
    original: &[u8],
    options: &SignOptions,
    signing_time: DateTime<Utc>,
    chain: &[Vec<u8>],
    evidence: &RetryEvidence,
) -> Result<(), String> {
    if evidence.cms_der.len() <= 16 * 1024 {
        return Err("first CMS fits its placeholder".into());
    }
    let (pdf_time, der_time) = signing_times(signing_time)?;
    let expected = pdf_lib::assemble(original, options, &pdf_time, 16 * 1024)?;
    if evidence.byte_range != expected.range || evidence.revision != expected.file {
        return Err("first revision differs from native request".into());
    }
    let [_, first, second, tail] = expected.range;
    if second.checked_add(tail) != Some(evidence.revision.len()) || first > second {
        return Err("invalid first byte range".into());
    }
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    hash.update(&evidence.revision[..first]);
    hash.update(&evidence.revision[second..]);
    let attributes = cms::attributes(hash.finish().as_ref(), &der_time);
    if evidence.attributes_der != attributes {
        return Err("first attributes differ from native request".into());
    }
    cms::verify(&evidence.cms_der, chain, &attributes, &evidence.signature)
}

/// Request-bound signing capability. It is only constructed inside the owned
/// signing worker, after the parent has authorized the captured request.
pub(super) struct Session {
    request: Option<Request>,
    state: State,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        original: Vec<u8>,
        options: SignOptions,
        signing_time: DateTime<Utc>,
        chain: Vec<Vec<u8>>,
        timeout: Option<Duration>,
        guard: Box<dyn Fn() -> Result<(), String> + Send>,
        admit: Admit,
        signer: Signer,
    ) -> Result<Self, String> {
        let cert = super::certificate(chain.first().ok_or("missing signer certificate")?)?;
        if cert.public_key().map_err(|error| error.to_string())?.id() != openssl::pkey::Id::RSA {
            return Err("unsupported signing certificate".into());
        }
        let deadline = timeout
            .map(|timeout| {
                Instant::now()
                    .checked_add(timeout)
                    .ok_or("invalid signing timeout")
            })
            .transpose()?;
        let (pdf_time, der_time) = signing_times(signing_time)?;
        Ok(Self {
            request: Some(Request {
                original,
                options,
                pdf_time,
                der_time,
                chain,
                deadline,
                guard,
                admit,
                signer,
            }),
            state: State::ReadyFirst,
        })
    }

    /// Assemble and sign only the immutable native request. No revision,
    /// attributes or digest is supplied by the service or renderer.
    pub(super) fn sign(mut self) -> Result<Vec<u8>, String> {
        if !matches!(self.state, State::ReadyFirst) {
            return Err("signing session is not ready".into());
        }
        self.state = State::Terminal;
        let mut request = self.request.take().ok_or("signing session is closed")?;
        let mut retry_evidence: Option<RetryEvidence> = None;
        for capacity in [16 * 1024, 48 * 1024] {
            authorize(&request)?;
            let mut assembled = pdf_lib::assemble(
                &request.original,
                &request.options,
                &request.pdf_time,
                capacity,
            )?;
            let [_, first, second, tail] = assembled.range;
            if second.checked_add(tail) != Some(assembled.file.len()) || first > second {
                return Err("invalid native signing byte range".into());
            }
            let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
            hash.update(&assembled.file[..first]);
            hash.update(&assembled.file[second..]);
            let digest = hash.finish();
            let attributes = cms::attributes(digest.as_ref(), &request.der_time);
            let digest = ring::digest::digest(&ring::digest::SHA256, &attributes);
            let digest: &[u8; 32] = digest
                .as_ref()
                .try_into()
                .map_err(|_| "invalid signing digest")?;
            authorize(&request)?;
            (request.admit)(match retry_evidence.as_ref() {
                Some(evidence) => Admission::Retry(evidence),
                None => Admission::First,
            })?;
            authorize(&request)?;
            let signature = (request.signer)(digest)?;
            authorize(&request)?;
            if !verify_signature(
                &request.chain[0],
                DigestAlgorithm::Sha256,
                &attributes,
                &signature,
            )? {
                return Err("native signer returned an invalid signature".into());
            }
            let cms = cms::build(&request.chain, &attributes, &signature)?;
            cms::verify(&cms, &request.chain, &attributes, &signature)?;
            authorize(&request)?;
            if cms.len() > capacity {
                if capacity == 16 * 1024 {
                    retry_evidence = Some(RetryEvidence {
                        revision: assembled.file,
                        byte_range: assembled.range,
                        attributes_der: attributes,
                        signature,
                        cms_der: cms,
                    });
                    continue;
                }
                return Err("signature exceeds final placeholder".into());
            }
            let start = first.checked_add(1).ok_or("invalid signature range")?;
            let end = start
                .checked_add(
                    cms.len()
                        .checked_mul(2)
                        .ok_or("signature length overflow")?,
                )
                .ok_or("signature length overflow")?;
            let destination = assembled
                .file
                .get_mut(start..end)
                .ok_or("invalid signature range")?;
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            for (byte, target) in cms.iter().zip(destination.chunks_exact_mut(2)) {
                target[0] = HEX[usize::from(byte >> 4)];
                target[1] = HEX[usize::from(byte & 15)];
            }
            authorize(&request)?;
            return Ok(assembled.file);
        }
        Err("signature exceeds final placeholder".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::bn::BigNum;
    use openssl::hash::MessageDigest;
    use openssl::pkey::PKey;
    use openssl::rsa::Rsa;
    use openssl::x509::{X509, X509NameBuilder};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn certificate() -> Vec<u8> {
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "Synthetic capability")
            .unwrap();
        let name = name.build();
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
            .unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(&Asn1Time::from_unix(0).unwrap())
            .unwrap();
        cert.set_not_after(&Asn1Time::from_unix(2_000_000_000).unwrap())
            .unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        cert.build().to_der().unwrap()
    }

    #[test]
    fn expired_requests_never_reach_signer() {
        let count = Arc::new(AtomicUsize::new(0));
        let signer = |count: Arc<AtomicUsize>| {
            Box::new(move |_: &[u8; 32]| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }) as Box<dyn FnMut(&[u8; 32]) -> Result<Vec<u8>, String> + Send>
        };
        let time = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let expired = Session::new(
            Vec::new(),
            SignOptions::default(),
            time,
            vec![certificate()],
            Some(Duration::ZERO),
            Box::new(|| Ok(())),
            Box::new(|_| Ok(())),
            signer(count.clone()),
        )
        .unwrap();
        assert!(expired.sign().is_err());
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_assembly_rejects_invalid_input_before_using_signer() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let signer = Box::new(move |_: &[u8; 32]| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        });
        let time = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let session = Session::new(
            b"%PDF-1.7\nbroken".to_vec(),
            SignOptions::default(),
            time,
            vec![certificate()],
            None,
            Box::new(|| Ok(())),
            Box::new(|_| Ok(())),
            signer,
        )
        .unwrap();
        assert!(session.sign().is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let observed = calls.clone();
        let signer = Box::new(move |_: &[u8; 32]| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        });
        let options = SignOptions {
            visible: Some(shell_contract::pdf_sign_options::VisibleSignature {
                page_index: 0.0,
                position: "bottom-right".into(),
                width: 80.0,
                height: 40.0,
                png: b"invalid".to_vec(),
            }),
            ..Default::default()
        };
        let session = Session::new(
            pdf_lib::synthetic_classic_pdf(),
            options,
            time,
            vec![certificate()],
            None,
            Box::new(|| Ok(())),
            Box::new(|_| Ok(())),
            signer,
        )
        .unwrap();
        assert!(session.sign().is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn document_revoke_is_checked_at_provider_boundary_and_after_return() {
        let time = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let checks = Arc::new(AtomicUsize::new(0));
        let observed = checks.clone();
        let guard = Box::new(move || {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err("document revoked".into())
            }
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let signer = Box::new(move |_: &[u8; 32]| {
            called.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        });
        let session = Session::new(
            pdf_lib::synthetic_classic_pdf(),
            SignOptions::default(),
            time,
            vec![certificate()],
            None,
            guard,
            Box::new(|_| Ok(())),
            signer,
        )
        .unwrap();
        assert!(session.sign().is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let called = calls.clone();
        let signer = Box::new(move |_: &[u8; 32]| {
            called.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        });
        let admit: Admit =
            Box::new(|_| Err("admission revoked".into()));
        let session = Session::new(
            pdf_lib::synthetic_classic_pdf(),
            SignOptions::default(),
            time,
            vec![certificate()],
            None,
            Box::new(|| Ok(())),
            admit,
            signer,
        )
        .unwrap();
        assert!(session.sign().is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let active = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let grant = active.clone();
        let guard = Box::new(move || {
            if grant.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("document revoked".into())
            }
        });
        let signer = Box::new(move |_: &[u8; 32]| {
            active.store(false, Ordering::SeqCst);
            Ok(Vec::new())
        });
        let session = Session::new(
            pdf_lib::synthetic_classic_pdf(),
            SignOptions::default(),
            time,
            vec![certificate()],
            None,
            guard,
            Box::new(|_| Ok(())),
            signer,
        )
        .unwrap();
        assert!(session.sign().is_err());
    }

    #[test]
    fn native_assembly_signs_only_its_own_request_with_a_synthetic_key() {
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "Synthetic signer").unwrap();
        let name = name.build();
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_serial_number(&BigNum::from_u32(7).unwrap().to_asn1_integer().unwrap())
            .unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(&Asn1Time::from_unix(0).unwrap())
            .unwrap();
        cert.set_not_after(&Asn1Time::from_unix(2_000_000_000).unwrap())
            .unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        let retry_key = key.clone();
        let second_key = key.clone();
        let cert = cert.build().to_der().unwrap();
        let signer = Box::new(move |digest: &[u8; 32]| {
            let mut context =
                openssl::pkey_ctx::PkeyCtx::new(&key).map_err(|error| error.to_string())?;
            context.sign_init().map_err(|error| error.to_string())?;
            context
                .set_rsa_padding(openssl::rsa::Padding::PKCS1)
                .map_err(|error| error.to_string())?;
            context
                .set_signature_md(openssl::md::Md::sha256())
                .map_err(|error| error.to_string())?;
            let mut signature = Vec::new();
            context
                .sign_to_vec(digest, &mut signature)
                .map_err(|error| error.to_string())?;
            Ok(signature)
        });
        let original = pdf_lib::synthetic_classic_pdf();
        let time = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let session = Session::new(
            original.clone(),
            SignOptions::default(),
            time,
            vec![cert.clone()],
            None,
            Box::new(|| Ok(())),
            Box::new(|_| Ok(())),
            signer,
        )
        .unwrap();
        let signed = session.sign().unwrap();
        assert!(signed.starts_with(&original));
        assert!(
            signed
                .windows(b"/ByteRange [0 ".len())
                .any(|part| part == b"/ByteRange [0 ")
        );
        let parsed = pdf_lib::Parser::new(&signed).document().unwrap();
        assert!(parsed.objects.values().any(|object| matches!(object, pdf_lib::Object::Dictionary(dict) if dict.iter().any(|(key, value)| key == b"Type" && *value == pdf_lib::Object::Name(b"Sig".to_vec())))));
        let signer = Box::new(move |digest: &[u8; 32]| {
            let mut context =
                openssl::pkey_ctx::PkeyCtx::new(&second_key).map_err(|error| error.to_string())?;
            context.sign_init().map_err(|error| error.to_string())?;
            context
                .set_rsa_padding(openssl::rsa::Padding::PKCS1)
                .map_err(|error| error.to_string())?;
            context
                .set_signature_md(openssl::md::Md::sha256())
                .map_err(|error| error.to_string())?;
            let mut signature = Vec::new();
            context
                .sign_to_vec(digest, &mut signature)
                .map_err(|error| error.to_string())?;
            Ok(signature)
        });
        let second = Session::new(
            signed.clone(),
            SignOptions::default(),
            time,
            vec![cert.clone()],
            None,
            Box::new(|| Ok(())),
            Box::new(|_| Ok(())),
            signer,
        )
        .unwrap()
        .sign()
        .unwrap();
        assert!(second.starts_with(&signed));
        let certify = SignOptions {
            certify_level: Some(1),
            ..Default::default()
        };
        let signer = Box::new(|_: &[u8; 32]| -> Result<Vec<u8>, String> {
            panic!("already signed document must not be certified")
        });
        assert!(
            Session::new(
                signed,
                certify,
                time,
                vec![cert.clone()],
                None,
                Box::new(|| Ok(())),
                Box::new(|_| Ok(())),
                signer
            )
            .unwrap()
            .sign()
            .is_err()
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let signer = Box::new(move |digest: &[u8; 32]| {
            observed.fetch_add(1, Ordering::SeqCst);
            let mut context =
                openssl::pkey_ctx::PkeyCtx::new(&retry_key).map_err(|error| error.to_string())?;
            context.sign_init().map_err(|error| error.to_string())?;
            context
                .set_rsa_padding(openssl::rsa::Padding::PKCS1)
                .map_err(|error| error.to_string())?;
            context
                .set_signature_md(openssl::md::Md::sha256())
                .map_err(|error| error.to_string())?;
            let mut signature = Vec::new();
            context
                .sign_to_vec(digest, &mut signature)
                .map_err(|error| error.to_string())?;
            Ok(signature)
        });
        let admissions = Arc::new(AtomicUsize::new(0));
        let observed = admissions.clone();
        let trusted_original = original.clone();
        let trusted_chain = vec![cert.clone(); 32];
        let verify_chain = trusted_chain.clone();
        let admit: Admit =
            Box::new(move |event| {
                let number = observed.fetch_add(1, Ordering::SeqCst);
                match event {
                    Admission::First => assert_eq!(number, 0),
                    Admission::Retry(evidence) => {
                        assert_eq!(number, 1);
                        let options = SignOptions::default();
                        assert!(
                            verify_first_retry(
                                &trusted_original,
                                &options,
                                time,
                                &verify_chain,
                                evidence
                            )
                            .is_ok()
                        );
                        let mut changed = evidence.clone();
                        changed.revision[0] ^= 1;
                        assert!(
                            verify_first_retry(
                                &trusted_original,
                                &options,
                                time,
                                &verify_chain,
                                &changed
                            )
                            .is_err()
                        );
                        changed = evidence.clone();
                        changed.byte_range[1] += 1;
                        assert!(
                            verify_first_retry(
                                &trusted_original,
                                &options,
                                time,
                                &verify_chain,
                                &changed
                            )
                            .is_err()
                        );
                        changed = evidence.clone();
                        changed.attributes_der[0] ^= 1;
                        assert!(
                            verify_first_retry(
                                &trusted_original,
                                &options,
                                time,
                                &verify_chain,
                                &changed
                            )
                            .is_err()
                        );
                        changed = evidence.clone();
                        changed.signature[0] ^= 1;
                        assert!(
                            verify_first_retry(
                                &trusted_original,
                                &options,
                                time,
                                &verify_chain,
                                &changed
                            )
                            .is_err()
                        );
                        changed = evidence.clone();
                        changed.cms_der[0] ^= 1;
                        assert!(
                            verify_first_retry(
                                &trusted_original,
                                &options,
                                time,
                                &verify_chain,
                                &changed
                            )
                            .is_err()
                        );
                        changed = evidence.clone();
                        changed.cms_der.truncate(16 * 1024);
                        assert!(
                            verify_first_retry(
                                &trusted_original,
                                &options,
                                time,
                                &verify_chain,
                                &changed
                            )
                            .is_err()
                        );
                        assert!(
                            verify_first_retry(
                                &trusted_original,
                                &options,
                                time + chrono::Duration::seconds(1),
                                &verify_chain,
                                evidence
                            )
                            .is_err()
                        );
                        assert!(
                            verify_first_retry(
                                &trusted_original,
                                &options,
                                time,
                                &[certificate()],
                                evidence
                            )
                            .is_err()
                        );
                    }
                }
                Ok(())
            });
        let session = Session::new(
            original,
            SignOptions::default(),
            time,
            trusted_chain,
            None,
            Box::new(|| Ok(())),
            admit,
            signer,
        )
        .unwrap();
        assert!(session.sign().is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(admissions.load(Ordering::SeqCst), 2);
    }
}
