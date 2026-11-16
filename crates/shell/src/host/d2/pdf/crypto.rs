mod cms;
pub(super) mod pdf_lib;
mod pkcs12;
mod session;
mod sign_worker_verify;
mod worker;
pub(crate) use worker::factory::{
    Authority as SigningAuthority, ProtectedTokenInteraction,
    open as open_signing_session_with_authority,
};

pub(crate) fn run_sign_worker(output: std::fs::File) -> Result<(), String> {
    worker::run_sign_worker(output)
}

use std::collections::HashSet;

use main_core::pdf::system_certificates::CertSummary;
use openssl::asn1::{Asn1Time, Asn1TimeRef};
use openssl::hash::MessageDigest;
use openssl::pkey::Id;
use openssl::rsa::Padding;
use openssl::sign::Verifier;
use openssl::x509::{X509, X509NameRef};
use shell_contract::IpcValue;
use shell_contract::pdf_sign_options::SignOptions;
use shell_ipc_d2::pdf::editor::hosts::{CertMaterial, SignFailure};

fn certificate(input: &[u8]) -> Result<X509, String> {
    X509::from_der(input)
        .or_else(|_| X509::from_pem(input))
        .map_err(|e| e.to_string())
}

fn name(name: &X509NameRef) -> Result<String, String> {
    name.entries()
        .map(|entry| {
            let label = entry.object().nid().short_name().unwrap_or("UNDEF");
            let value = entry.data().as_utf8().map_err(|e| e.to_string())?;
            Ok(format!("{label}={value}"))
        })
        .collect::<Result<Vec<_>, String>>()
        .map(|parts| parts.join("\n"))
}

fn common_name(name: &str) -> String {
    name.lines()
        .find_map(|line| line.strip_prefix("CN="))
        .map(str::to_owned)
        .unwrap_or_else(|| name.lines().collect::<Vec<_>>().join(", "))
}

fn iso_time(time: &Asn1TimeRef) -> Result<String, String> {
    let epoch = Asn1Time::from_unix(0).map_err(|e| e.to_string())?;
    let diff = epoch.diff(time).map_err(|e| e.to_string())?;
    let seconds = i64::from(diff.days) * 86_400 + i64::from(diff.secs);
    let date = chrono::DateTime::from_timestamp(seconds, 0).ok_or("invalid certificate date")?;
    Ok(date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn is_self_signed(cert: &X509) -> bool {
    let same_name = match (name(cert.subject_name()), name(cert.issuer_name())) {
        (Ok(subject), Ok(issuer)) => subject == issuer,
        _ => false,
    };
    same_name
        && cert
            .public_key()
            .ok()
            .and_then(|key| cert.verify(&key).ok())
            .unwrap_or(false)
}

fn describe(cert: &X509) -> Result<IpcValue, String> {
    let subject = name(cert.subject_name())?;
    let issuer = name(cert.issuer_name())?;
    let key = cert.public_key().map_err(|e| e.to_string())?;
    let algorithm = match key.id() {
        Id::RSA => format!("RSA {}", key.bits()),
        Id::EC => {
            let ec = key.ec_key().map_err(|e| e.to_string())?;
            let curve = ec
                .group()
                .curve_name()
                .and_then(|n| n.short_name().ok())
                .unwrap_or("");
            format!("EC {curve}").trim_end().to_owned()
        }
        Id::RSA_PSS => "RSA-PSS".to_owned(),
        Id::DSA => "DSA".to_owned(),
        Id::DH => "DH".to_owned(),
        Id::ED25519 => "ED25519".to_owned(),
        Id::ED448 => "ED448".to_owned(),
        Id::X25519 => "X25519".to_owned(),
        Id::X448 => "X448".to_owned(),
        _ => "UNKNOWN".to_owned(),
    };
    let digest = cert
        .digest(MessageDigest::sha256())
        .map_err(|e| e.to_string())?;
    let fingerprint = digest
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    let serial = cert
        .serial_number()
        .to_bn()
        .and_then(|n| n.to_hex_str())
        .map_err(|e| e.to_string())?
        .to_string();
    let text = |value: String| IpcValue::String(value);
    Ok(IpcValue::Object(vec![
        ("commonName".into(), text(common_name(&subject))),
        ("subject".into(), text(subject)),
        ("issuerCommonName".into(), text(common_name(&issuer))),
        ("issuer".into(), text(issuer)),
        ("serialNumber".into(), text(serial)),
        ("validFrom".into(), text(iso_time(cert.not_before())?)),
        ("validTo".into(), text(iso_time(cert.not_after())?)),
        ("fingerprint".into(), text(fingerprint)),
        ("keyAlgorithm".into(), text(algorithm)),
        ("selfSigned".into(), IpcValue::Bool(is_self_signed(cert))),
    ]))
}

pub(super) fn describe_certificate(input: &[u8]) -> Option<CertSummary> {
    let cert = certificate(input).ok()?;
    let subject = name(cert.subject_name()).ok()?;
    let issuer = name(cert.issuer_name()).ok()?;
    Some(CertSummary {
        common_name: common_name(&subject),
        issuer_common_name: common_name(&issuer),
        valid_to: iso_time(cert.not_after()).ok()?,
        is_rsa: cert.public_key().ok()?.id() == Id::RSA,
    })
}

pub(super) fn certificate_info(input: &[u8]) -> Result<IpcValue, String> {
    describe(&certificate(input)?)
}

pub(super) fn open_certificate(material: &CertMaterial) -> Result<Vec<IpcValue>, String> {
    match material {
        CertMaterial::P12 { p12, password } => {
            let identity = pkcs12::open(p12, password, None)?;
            identity
                .chain_der
                .iter()
                .map(|der| certificate_info(der).map_err(|_| "cert-invalid".into()))
                .collect()
        }
        CertMaterial::InPlace(signer) => {
            let first = signer.chain_der.first().ok_or("cert-invalid")?;
            let key = certificate(first)
                .and_then(|cert| cert.public_key().map_err(|e| e.to_string()))
                .map_err(|_| "cert-invalid")?;
            if key.id() != Id::RSA {
                return Err("cert-unsupported-key".into());
            }
            signer
                .chain_der
                .iter()
                .map(|der| certificate_info(der).map_err(|_| "cert-invalid".into()))
                .collect()
        }
    }
}

fn signature_failure(error: String) -> SignFailure {
    match error.as_str() {
        "cert-password"
        | "cert-invalid"
        | "cert-no-key"
        | "cert-unsupported-key"
        | "cert-not-exportable"
        | "cert-store-tool-missing" => SignFailure::Code(error),
        "PDF is encrypted" => SignFailure::Code("pdf-encrypted".into()),
        _ => SignFailure::Code("sign-failed".into()),
    }
}

#[derive(Clone, Copy)]
pub(super) enum DigestAlgorithm {
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

pub(super) fn verify_signature(
    cert_der: &[u8],
    algorithm: DigestAlgorithm,
    message: &[u8],
    signature: &[u8],
) -> Result<bool, String> {
    let cert = certificate(cert_der)?;
    let key = cert.public_key().map_err(|e| e.to_string())?;
    if key.id() != Id::RSA && key.id() != Id::EC {
        return Err("unsupported-algorithm".into());
    }
    let md = match algorithm {
        DigestAlgorithm::Sha1 => MessageDigest::sha1(),
        DigestAlgorithm::Sha224 => MessageDigest::sha224(),
        DigestAlgorithm::Sha256 => MessageDigest::sha256(),
        DigestAlgorithm::Sha384 => MessageDigest::sha384(),
        DigestAlgorithm::Sha512 => MessageDigest::sha512(),
    };
    let mut verifier = Verifier::new(md, &key).map_err(|e| e.to_string())?;
    if key.id() == Id::RSA {
        verifier
            .set_rsa_padding(Padding::PKCS1)
            .map_err(|e| e.to_string())?;
    }
    verifier.update(message).map_err(|e| e.to_string())?;
    verifier.verify(signature).map_err(|e| e.to_string())
}

/// Follows the original PDF verifier's name-and-signature links, without
/// substituting platform path-building or revocation policy for its rules.
pub(super) fn verified_chain(
    signer_der: &[u8],
    pool_der: &[Vec<u8>],
) -> Result<Vec<Vec<u8>>, String> {
    let signer = certificate(signer_der)?;
    let pool: Vec<_> = pool_der
        .iter()
        .filter_map(|der| certificate(der).ok().map(|cert| (der, cert)))
        .collect();
    let mut used = HashSet::new();
    let mut chain = vec![signer_der.to_vec()];
    let mut current = signer;
    for _ in 0..10 {
        if is_self_signed(&current) {
            break;
        }
        let issuer = name(current.issuer_name())?;
        let next = pool.iter().enumerate().find(|(i, (_, candidate))| {
            !used.contains(i)
                && name(candidate.subject_name()).ok().as_deref() == Some(issuer.as_str())
                && candidate
                    .public_key()
                    .ok()
                    .and_then(|key| current.verify(&key).ok())
                    .unwrap_or(false)
        });
        let Some((index, (der, cert))) = next else {
            break;
        };
        used.insert(index);
        chain.push((*der).clone());
        current = cert.clone();
    }
    Ok(chain)
}

fn trusted_by_roots<'a>(chain: &[Vec<u8>], roots: impl IntoIterator<Item = &'a [u8]>) -> bool {
    let roots: HashSet<Vec<u8>> = roots
        .into_iter()
        .filter_map(|der| X509::from_der(der).ok())
        .filter_map(|cert| {
            cert.digest(MessageDigest::sha256())
                .ok()
                .map(|digest| digest.to_vec())
        })
        .collect();
    chain
        .iter()
        .filter_map(|der| X509::from_der(der).ok())
        .filter_map(|cert| cert.digest(MessageDigest::sha256()).ok())
        .any(|digest| roots.contains(digest.as_ref()))
}

pub(super) fn verified_chain_and_trust_with_roots<'a>(
    signer: &[u8],
    pool: &[Vec<u8>],
    roots: impl IntoIterator<Item = &'a [u8]>,
) -> Result<(Vec<Vec<u8>>, bool), String> {
    let chain = verified_chain(signer, pool)?;
    let trusted = trusted_by_roots(&chain, roots);
    Ok((chain, trusted))
}

pub(super) fn verified_chain_and_trust(
    signer: &[u8],
    pool: &[Vec<u8>],
) -> Result<(Vec<Vec<u8>>, bool), String> {
    let system = rustls_native_certs::load_native_certs();
    let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|der| der.as_ref())
        .chain(system.certs.iter().map(|der| der.as_ref()));
    verified_chain_and_trust_with_roots(signer, pool, roots)
}

/// Checks the byte range and signed attributes. This alone does not authorize
/// signing: the caller must also validate the incremental object changes.
pub(super) fn signing_revision_digest(
    original: &[u8],
    revision: &[u8],
    range: [usize; 4],
    attributes_der: &[u8],
    expected_time_der: &[u8],
    attempt: u8,
) -> Result<[u8; 32], String> {
    let placeholder = match attempt {
        1 => 32_768,
        2 => 98_304,
        _ => return Err("invalid signature attempt".into()),
    };
    if original.len() > 400 * 1024 * 1024 || !revision.starts_with(original) {
        return Err("signature revision does not preserve the original PDF".into());
    }
    let [start, first_len, second_start, second_len] = range;
    if start != 0
        || first_len < original.len()
        || second_start.checked_sub(first_len) != Some(placeholder + 2)
        || second_start.checked_add(second_len) != Some(revision.len())
    {
        return Err("invalid signature byte range".into());
    }
    let gap = revision
        .get(first_len..second_start)
        .ok_or("invalid signature byte range")?;
    if gap.first() != Some(&b'<')
        || gap.last() != Some(&b'>')
        || !gap[1..gap.len() - 1].iter().all(|byte| *byte == b'0')
    {
        return Err("invalid signature placeholder".into());
    }
    let marker =
        format!("/ByteRange [0 {first_len:010} {second_start:010} {second_len:010}] /Contents ");
    if !revision[..first_len].ends_with(marker.as_bytes()) {
        return Err("signature byte range does not match the revision".into());
    }
    let attributes = yasna::parse_der(attributes_der, |reader| {
        reader.collect_set_of(|reader| {
            reader.read_sequence(|seq| {
                let oid = seq.next().read_oid()?;
                let values = seq.next().collect_set_of(|value| value.read_der())?;
                Ok((oid, values))
            })
        })
    })
    .map_err(|_| "invalid signed attributes".to_owned())?;
    let mut content_type = false;
    let mut message_digest = None;
    let mut signing_time = false;
    for (oid, values) in attributes {
        if values.len() != 1 {
            return Err("invalid signed attributes".into());
        }
        match oid.components().as_slice() {
            [1, 2, 840, 113549, 1, 9, 3] if !content_type => {
                let value = yasna::parse_der(&values[0], |reader| reader.read_oid())
                    .map_err(|_| "invalid signed attributes")?;
                if value.components().as_slice() != [1, 2, 840, 113549, 1, 7, 1] {
                    return Err("invalid signed content type".into());
                }
                content_type = true;
            }
            [1, 2, 840, 113549, 1, 9, 4] if message_digest.is_none() => {
                message_digest = Some(
                    yasna::parse_der(&values[0], |reader| reader.read_bytes())
                        .map_err(|_| "invalid signed attributes")?,
                );
            }
            [1, 2, 840, 113549, 1, 9, 5] if !signing_time => {
                if values[0] != expected_time_der {
                    return Err("unexpected signing time".into());
                }
                signing_time = true;
            }
            _ => return Err("invalid signed attributes".into()),
        }
    }
    let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
    hasher.update(&revision[..first_len]);
    hasher.update(&revision[second_start..]);
    let actual = hasher.finish();
    if !content_type || !signing_time || message_digest.as_deref() != Some(actual.as_ref()) {
        return Err("signed attributes do not match the PDF".into());
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, attributes_der);
    digest
        .as_ref()
        .try_into()
        .map_err(|_| "invalid signature digest".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::bn::BigNum;
    use openssl::pkey::PKey;
    use openssl::rsa::Rsa;
    use openssl::sign::Signer;
    use openssl::x509::X509NameBuilder;

    #[test]
    fn signing_denies_unbound_or_cancelled_owner_before_loading_key() {
        let owner = shell_contract::ipc::NativeRequestOwner {
            view: shell_contract::ViewId(41),
            context: shell_contract::ipc::NativeContext::new(()),
        };
        let cancel = shell_contract::runtime::CancelToken::default();
        let authority = super::signed_commit::NativeSigningAuthority::new();
        assert!(!authority.is_current(&owner, &cancel));
        cancel.cancel();
        assert!(!authority.is_current(&owner, &cancel));
    }

    #[test]
    fn verifies_synthetic_certificate_and_rejects_changed_message() {
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut subject = X509NameBuilder::new().unwrap();
        subject
            .append_entry_by_text("CN", "Synthetic signer")
            .unwrap();
        let subject = subject.build();
        let mut builder = X509::builder().unwrap();
        builder.set_version(2).unwrap();
        let serial = BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap();
        builder.set_serial_number(&serial).unwrap();
        builder.set_subject_name(&subject).unwrap();
        builder.set_issuer_name(&subject).unwrap();
        builder.set_pubkey(&key).unwrap();
        builder
            .set_not_before(&Asn1Time::from_unix(0).unwrap())
            .unwrap();
        builder
            .set_not_after(&Asn1Time::from_unix(2_000_000_000).unwrap())
            .unwrap();
        builder.sign(&key, MessageDigest::sha256()).unwrap();
        let der = builder.build().to_der().unwrap();
        let summary = describe_certificate(&der).unwrap();
        assert_eq!(summary.common_name, "Synthetic signer");
        assert!(summary.is_rsa);
        let mut signer = Signer::new(MessageDigest::sha256(), &key).unwrap();
        signer.update(b"content").unwrap();
        let signature = signer.sign_to_vec().unwrap();
        assert!(verify_signature(&der, DigestAlgorithm::Sha256, b"content", &signature).unwrap());
        assert!(
            !verify_signature(&der, DigestAlgorithm::Sha256, b"changed", &signature)
                .unwrap_or(false)
        );
        let (chain, trusted) =
            verified_chain_and_trust_with_roots(&der, &[], [der.as_slice()]).unwrap();
        assert_eq!(chain, vec![der.clone()]);
        assert!(trusted);
        assert!(
            !verified_chain_and_trust_with_roots(&der, &[], std::iter::empty())
                .unwrap()
                .1
        );
    }

    #[test]
    fn signing_revision_binds_ranges_attributes_and_original() {
        use yasna::models::ObjectIdentifier;
        let original = b"%PDF-1.7\n";
        let tail = b"\nendobj\n%%EOF\n";
        let template = "/ByteRange [0 0000000000 0000000000 0000000000] /Contents ";
        let first = original.len() + template.len();
        let second = first + 32_768 + 2;
        let marker = format!(
            "/ByteRange [0 {first:010} {second:010} {:010}] /Contents ",
            tail.len()
        );
        let mut revision = original.to_vec();
        revision.extend_from_slice(marker.as_bytes());
        revision.push(b'<');
        revision.extend(std::iter::repeat_n(b'0', 32_768));
        revision.push(b'>');
        revision.extend_from_slice(tail);
        let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
        hasher.update(&revision[..first]);
        hasher.update(&revision[second..]);
        let content_digest = hasher.finish();
        let time = b"\x17\x0d250101000000Z";
        let attrs = yasna::construct_der(|writer| {
            writer.write_set_of(|set| {
                set.next().write_sequence(|seq| {
                    seq.next()
                        .write_oid(&ObjectIdentifier::from_slice(&[1, 2, 840, 113549, 1, 9, 3]));
                    seq.next().write_set_of(|set| {
                        set.next()
                            .write_oid(&ObjectIdentifier::from_slice(&[1, 2, 840, 113549, 1, 7, 1]))
                    });
                });
                set.next().write_sequence(|seq| {
                    seq.next()
                        .write_oid(&ObjectIdentifier::from_slice(&[1, 2, 840, 113549, 1, 9, 4]));
                    seq.next()
                        .write_set_of(|set| set.next().write_bytes(content_digest.as_ref()));
                });
                set.next().write_sequence(|seq| {
                    seq.next()
                        .write_oid(&ObjectIdentifier::from_slice(&[1, 2, 840, 113549, 1, 9, 5]));
                    seq.next().write_set_of(|set| set.next().write_der(time));
                });
            })
        });
        let range = [0, first, second, tail.len()];
        assert!(signing_revision_digest(original, &revision, range, &attrs, time, 1).is_ok());
        assert!(signing_revision_digest(original, &revision, range, &attrs, time, 2).is_err());
        assert!(signing_revision_digest(b"%PDF-other", &revision, range, &attrs, time, 1).is_err());
        assert!(signing_revision_digest(original, &revision, range, &attrs, b"bad", 1).is_err());
        revision[second] ^= 1;
        assert!(signing_revision_digest(original, &revision, range, &attrs, time, 1).is_err());
    }

    #[test]
    fn strict_diagnostic_rejects_invalid_xref_object_instead_of_skipping_it() {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let one = pdf.len();
        pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        let two = pdf.len();
        pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        let xref = pdf.len();
        pdf.extend_from_slice(format!(
            "xref\n0 3\n0000000000 65535 f \n{one:010} 00000 n \n{two:010} 00000 n \ntrailer\n<< /Size 3 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        ).as_bytes());
        let strict = || lopdf::LoadOptions {
            strict: true,
            ..Default::default()
        };
        assert!(lopdf::Document::load_mem_with_options(&pdf, strict()).is_ok());
        pdf[one] = b'9';
        assert!(lopdf::Document::load_mem_with_options(&pdf, strict()).is_err());
    }

    #[test]
    fn strict_xref_diagnostic_does_not_interpret_unindexed_redefinitions() {
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let one = pdf.len();
        pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Marker /Old >>\nendobj\n");
        pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Marker /New >>\nendobj\n");
        let xref = pdf.len();
        pdf.extend_from_slice(format!(
            "xref\n0 2\n0000000000 65535 f \n{one:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        ).as_bytes());
        let options = lopdf::LoadOptions {
            strict: true,
            ..Default::default()
        };
        let parsed = lopdf::Document::load_mem_with_options(&pdf, options).unwrap();
        let catalog = parsed.get_dictionary((1, 0)).unwrap();
        assert_eq!(catalog.get(b"Marker").unwrap().as_name().unwrap(), b"Old");
    }
}
