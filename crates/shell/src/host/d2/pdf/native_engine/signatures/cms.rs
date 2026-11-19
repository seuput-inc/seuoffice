//! Read-only CMS SignedData decoding. Verification remains in the caller.
use openssl::x509::X509;

#[derive(Clone, Copy)]
struct Element<'a> {
    tag: u8,
    body: &'a [u8],
    whole: &'a [u8],
}
fn element<'a>(input: &mut &'a [u8]) -> Result<Element<'a>, ()> {
    let bytes = *input;
    let tag = *bytes.first().ok_or(())?;
    let first = *bytes.get(1).ok_or(())?;
    let (length, header) = if first < 128 {
        (usize::from(first), 2)
    } else {
        let count = usize::from(first & 127);
        if count == 0 || count > 4 {
            return Err(());
        }
        let mut length = 0usize;
        for byte in bytes.get(2..2 + count).ok_or(())? {
            length = length
                .checked_mul(256)
                .and_then(|v| v.checked_add(usize::from(*byte)))
                .ok_or(())?;
        }
        (length, 2 + count)
    };
    let end = header.checked_add(length).ok_or(())?;
    let whole = bytes.get(..end).ok_or(())?;
    *input = &bytes[end..];
    Ok(Element {
        tag,
        body: &whole[header..],
        whole,
    })
}
fn children(node: Element<'_>) -> Result<Vec<Element<'_>>, ()> {
    let mut bytes = node.body;
    let mut out = Vec::new();
    while !bytes.is_empty() {
        out.push(element(&mut bytes)?);
    }
    Ok(out)
}
fn encode_length(length: usize, out: &mut Vec<u8>) {
    if length < 128 {
        out.push(length as u8)
    } else {
        let bytes = length.to_be_bytes();
        let at = bytes
            .iter()
            .position(|b| *b != 0)
            .unwrap_or(bytes.len() - 1);
        out.push(0x80 | (bytes.len() - at) as u8);
        out.extend_from_slice(&bytes[at..]);
    }
}
fn der(element: Element<'_>, tag: u8) -> Result<Vec<u8>, ()> {
    let body = if element.tag & 0x20 != 0 {
        let mut out = Vec::new();
        for child in children(element)? {
            out.extend(der(child, child.tag)?);
        }
        out
    } else {
        element.body.to_vec()
    };
    let mut out = vec![tag];
    encode_length(body.len(), &mut out);
    out.extend(body);
    Ok(out)
}
fn oid(element: Element<'_>) -> Result<String, ()> {
    if element.tag != 0x06 || element.body.is_empty() {
        return Err(());
    }
    let mut values = Vec::new();
    let mut value = 0u64;
    let mut open = false;
    for byte in element.body {
        value = value
            .checked_mul(128)
            .and_then(|n| n.checked_add(u64::from(byte & 127)))
            .ok_or(())?;
        open = true;
        if byte & 128 == 0 {
            values.push(value);
            value = 0;
            open = false;
        }
    }
    if open || values.is_empty() {
        return Err(());
    }
    let first = values[0];
    let a = if first < 40 {
        0
    } else if first < 80 {
        1
    } else {
        2
    };
    let b = first - a * 40;
    let mut out = format!("{a}.{b}");
    for value in &values[1..] {
        out.push('.');
        out.push_str(&value.to_string());
    }
    Ok(out)
}
fn unsigned(mut bytes: &[u8]) -> &[u8] {
    while bytes.first() == Some(&0) {
        bytes = &bytes[1..]
    }
    bytes
}

pub(super) struct SignedData {
    pub certificates: Vec<Vec<u8>>,
    pub signer: Option<usize>,
    pub digest_oid: String,
    pub signature_oid: String,
    pub signature: Vec<u8>,
    pub attributes: Option<Vec<u8>>,
    pub message_digest: Option<Vec<u8>>,
    pub signing_time: Option<String>,
}
fn time(element: Element<'_>) -> Result<String, ()> {
    let raw = std::str::from_utf8(element.body).map_err(|_| ())?;
    let asn = openssl::asn1::Asn1Time::from_str_x509(raw).map_err(|_| ())?;
    let epoch = openssl::asn1::Asn1Time::from_unix(0).map_err(|_| ())?;
    let diff = epoch.diff(&asn).map_err(|_| ())?;
    let seconds = i64::from(diff.days) * 86400 + i64::from(diff.secs);
    Ok(chrono::DateTime::from_timestamp(seconds, 0)
        .ok_or(())?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}
pub(super) fn parse(bytes: &[u8]) -> Result<SignedData, ()> {
    let mut input = bytes;
    let root = element(&mut input)?;
    if root.tag != 0x30 {
        return Err(());
    }
    let root_children = children(root)?;
    if oid(*root_children.first().ok_or(())?)? != "1.2.840.113549.1.7.2" {
        return Err(());
    }
    let wrapper = *root_children.get(1).ok_or(())?;
    let signed = *children(wrapper)?.first().ok_or(())?;
    let fields = children(signed)?;
    let mut certificates = Vec::new();
    let mut issuers = Vec::new();
    let mut signers = None;
    for (index, node) in fields.iter().copied().enumerate() {
        if node.tag == 0xa0 {
            for cert in children(node)? {
                if cert.tag != 0x30 {
                    continue;
                }
                if X509::from_der(cert.whole).is_ok() {
                    let tbs = *children(cert)?.first().ok_or(())?;
                    let parts = children(tbs)?;
                    let offset = usize::from(parts.first().is_some_and(|v| v.tag == 0xa0));
                    let issuer = der(*parts.get(offset + 2).ok_or(())?, 0x30)?;
                    certificates.push(cert.whole.to_vec());
                    issuers.push(issuer);
                }
            }
        } else if node.tag == 0x31 && index != 1 {
            signers = children(node)?.first().copied();
        }
    }
    let parts = children(signers.ok_or(())?)?;
    let sid = children(*parts.get(1).ok_or(())?)?;
    if sid.len() < 2 {
        return Err(());
    }
    let issuer = der(sid[0], sid[0].tag)?;
    let serial = sid[1].body;
    let digest_oid = oid(*children(*parts.get(2).ok_or(())?)?.first().ok_or(())?)?;
    let mut at = 3;
    let mut attributes = None;
    let mut message_digest = None;
    let mut signing_time = None;
    if parts.get(at).is_some_and(|node| node.tag == 0xa0) {
        let attrs = parts[at];
        attributes = Some(der(attrs, 0x31)?);
        for attr in children(attrs)? {
            let values = children(attr)?;
            if values.len() < 2 {
                continue;
            }
            let Some(value) = children(values[1])?.first().copied() else {
                continue;
            };
            match oid(values[0])?.as_str() {
                "1.2.840.113549.1.9.4" => message_digest = Some(value.body.to_vec()),
                "1.2.840.113549.1.9.5" => signing_time = Some(time(value)?),
                _ => {}
            }
        }
        at += 1;
    }
    let signature_oid = oid(*children(*parts.get(at).ok_or(())?)?.first().ok_or(())?)?;
    let signature = parts.get(at + 1).ok_or(())?.body.to_vec();
    let mut fallback = None;
    let mut signer = None;
    for (index, bytes) in certificates.iter().enumerate() {
        let Ok(cert) = X509::from_der(bytes) else {
            continue;
        };
        let Ok(actual) = cert.serial_number().to_bn().map(|n| n.to_vec()) else {
            continue;
        };
        if unsigned(&actual) != unsigned(serial) {
            continue;
        }
        if fallback.is_none() {
            fallback = Some(index)
        }
        if issuers[index] == issuer {
            signer = Some(index);
            break;
        }
    }
    Ok(SignedData {
        certificates,
        signer: signer.or(fallback),
        digest_oid,
        signature_oid,
        signature,
        attributes,
        message_digest,
        signing_time,
    })
}
