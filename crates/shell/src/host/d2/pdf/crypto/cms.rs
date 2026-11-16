use super::{DigestAlgorithm, certificate, verify_signature};

struct Element<'a> {
    tag: u8,
    whole: &'a [u8],
    body: &'a [u8],
}

fn element<'a>(data: &mut &'a [u8]) -> Result<Element<'a>, String> {
    let input = *data;
    let tag = *input.first().ok_or("truncated CMS")?;
    let first = *input.get(1).ok_or("truncated CMS")?;
    let (length, header) = if first & 0x80 == 0 {
        (usize::from(first), 2)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > std::mem::size_of::<usize>() {
            return Err("invalid CMS length".into());
        }
        let bytes = input.get(2..2 + count).ok_or("truncated CMS length")?;
        if bytes[0] == 0 {
            return Err("noncanonical CMS length".into());
        }
        let mut length = 0usize;
        for byte in bytes {
            length = length
                .checked_mul(256)
                .and_then(|n| n.checked_add(usize::from(*byte)))
                .ok_or("CMS length overflow")?;
        }
        if length < 128 {
            return Err("noncanonical CMS length".into());
        }
        (length, 2 + count)
    };
    let end = header.checked_add(length).ok_or("CMS length overflow")?;
    let whole = input.get(..end).ok_or("truncated CMS")?;
    *data = &input[end..];
    Ok(Element {
        tag,
        whole,
        body: &whole[header..],
    })
}

fn expect<'a>(data: &mut &'a [u8], tag: u8) -> Result<Element<'a>, String> {
    let value = element(data)?;
    if value.tag != tag {
        return Err("unexpected CMS element".into());
    }
    Ok(value)
}
fn end(data: &[u8]) -> Result<(), String> {
    if data.is_empty() {
        Ok(())
    } else {
        Err("unexpected CMS fields".into())
    }
}
fn oid(data: &mut &[u8], expected: &[u8]) -> Result<(), String> {
    if expect(data, 0x06)?.body != expected {
        return Err("unexpected CMS algorithm".into());
    }
    Ok(())
}
fn algorithm(data: &mut &[u8], expected: &[u8]) -> Result<(), String> {
    let mut sequence = expect(data, 0x30)?.body;
    oid(&mut sequence, expected)?;
    if !expect(&mut sequence, 0x05)?.body.is_empty() {
        return Err("invalid CMS algorithm parameters".into());
    }
    end(sequence)
}
fn one(data: &mut &[u8]) -> Result<(), String> {
    if expect(data, 0x02)?.body != [1] {
        return Err("invalid CMS version".into());
    }
    Ok(())
}
fn unsigned(mut data: &[u8]) -> &[u8] {
    while data.len() > 1 && data[0] == 0 {
        data = &data[1..];
    }
    data
}

/// Verify a returned forge CMS against the request-bound certificate chain,
/// authenticated attributes and the actual native signature.
pub(super) fn verify(
    cms: &[u8],
    chain: &[Vec<u8>],
    signed_attributes: &[u8],
    first_signature: &[u8],
) -> Result<(), String> {
    const SIGNED_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02];
    const DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x01];
    const SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
    const RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
    let signer = certificate(chain.first().ok_or("missing signer certificate")?)?;
    if signer.public_key().map_err(|error| error.to_string())?.id() != openssl::pkey::Id::RSA {
        return Err("unsupported CMS signer".into());
    }
    let mut input = cms;
    let mut root = expect(&mut input, 0x30)?.body;
    end(input)?;
    oid(&mut root, SIGNED_DATA)?;
    let mut explicit = expect(&mut root, 0xa0)?.body;
    end(root)?;
    let mut signed = expect(&mut explicit, 0x30)?.body;
    end(explicit)?;
    one(&mut signed)?;
    let mut algorithms = expect(&mut signed, 0x31)?.body;
    algorithm(&mut algorithms, SHA256)?;
    end(algorithms)?;
    let mut content = expect(&mut signed, 0x30)?.body;
    oid(&mut content, DATA)?;
    end(content)?;
    let mut certs = expect(&mut signed, 0xa0)?.body;
    for expected in chain {
        let expected = certificate(expected)?
            .to_der()
            .map_err(|error| error.to_string())?;
        if expect(&mut certs, 0x30)?.whole != expected.as_slice() {
            return Err("CMS certificate chain differs from native identity".into());
        }
    }
    end(certs)?;
    let mut signers = expect(&mut signed, 0x31)?.body;
    end(signed)?;
    let mut info = expect(&mut signers, 0x30)?.body;
    end(signers)?;
    one(&mut info)?;
    let mut identity = expect(&mut info, 0x30)?.body;
    let issuer = expect(&mut identity, 0x30)?.whole;
    if issuer
        != signer
            .issuer_name()
            .to_der()
            .map_err(|error| error.to_string())?
            .as_slice()
    {
        return Err("CMS signer issuer differs from native identity".into());
    }
    let serial = expect(&mut identity, 0x02)?.body;
    let expected_serial = signer
        .serial_number()
        .to_bn()
        .map_err(|error| error.to_string())?
        .to_vec();
    if unsigned(serial) != unsigned(&expected_serial) {
        return Err("CMS signer serial differs from native identity".into());
    }
    end(identity)?;
    algorithm(&mut info, SHA256)?;
    let attributes = expect(&mut info, 0xa0)?.whole;
    if signed_attributes.first() != Some(&0x31)
        || attributes.len() != signed_attributes.len()
        || attributes[1..] != signed_attributes[1..]
    {
        return Err("CMS signed attributes differ from native request".into());
    }
    algorithm(&mut info, RSA)?;
    let signature = expect(&mut info, 0x04)?.body;
    end(info)?;
    if signature != first_signature
        || !verify_signature(
            &chain[0],
            DigestAlgorithm::Sha256,
            signed_attributes,
            signature,
        )?
    {
        return Err("CMS signature differs from the native signature".into());
    }
    Ok(())
}

fn der(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len().saturating_add(10));
    out.push(tag);
    if body.len() < 128 {
        out.push(body.len() as u8);
    } else {
        let bytes = body.len().to_be_bytes();
        let start = bytes
            .iter()
            .position(|byte| *byte != 0)
            .unwrap_or(bytes.len() - 1);
        out.push(0x80 | (bytes.len() - start) as u8);
        out.extend_from_slice(&bytes[start..]);
    }
    out.extend_from_slice(body);
    out
}
fn sequence(parts: &[&[u8]]) -> Vec<u8> {
    der(0x30, &parts.concat())
}

/// Forge's authenticated attributes in their original requested order.
pub(super) fn attributes(digest: &[u8], time: &[u8]) -> Vec<u8> {
    const CONTENT_TYPE: &[u8] = &[6, 9, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 9, 3];
    const MESSAGE_DIGEST: &[u8] = &[6, 9, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 9, 4];
    const SIGNING_TIME: &[u8] = &[6, 9, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 9, 5];
    const DATA: &[u8] = &[6, 9, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 7, 1];
    let content_type = sequence(&[CONTENT_TYPE, &der(0x31, DATA)]);
    let message_digest = sequence(&[MESSAGE_DIGEST, &der(0x31, &der(0x04, digest))]);
    let signing_time = sequence(&[SIGNING_TIME, &der(0x31, time)]);
    der(0x31, &[content_type, message_digest, signing_time].concat())
}

/// Native detached CMS using the request-bound identity and signature.
pub(super) fn build(
    chain: &[Vec<u8>],
    attributes: &[u8],
    signature: &[u8],
) -> Result<Vec<u8>, String> {
    const SIGNED_DATA: &[u8] = &[6, 9, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 7, 2];
    const DATA: &[u8] = &[6, 9, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 7, 1];
    const SHA256: &[u8] = &[6, 9, 0x60, 0x86, 0x48, 1, 0x65, 3, 4, 2, 1];
    const RSA: &[u8] = &[6, 9, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 1, 1];
    let signer = certificate(chain.first().ok_or("missing signer certificate")?)?;
    let issuer = signer
        .issuer_name()
        .to_der()
        .map_err(|error| error.to_string())?;
    let mut serial = signer
        .serial_number()
        .to_bn()
        .map_err(|error| error.to_string())?
        .to_vec();
    if serial.is_empty() {
        serial.push(0);
    }
    if serial[0] & 0x80 != 0 {
        serial.insert(0, 0);
    }
    let identity = sequence(&[&issuer, &der(2, &serial)]);
    let hash_algorithm = sequence(&[SHA256, &[5, 0]]);
    let rsa_algorithm = sequence(&[RSA, &[5, 0]]);
    if attributes.first() != Some(&0x31) {
        return Err("invalid signed attributes".into());
    }
    let mut implicit = attributes.to_vec();
    implicit[0] = 0xa0;
    let info = sequence(&[
        &[2, 1, 1],
        &identity,
        &hash_algorithm,
        &implicit,
        &rsa_algorithm,
        &der(4, signature),
    ]);
    let content = sequence(&[DATA]);
    let certs = der(0xa0, &chain.concat());
    let signed = sequence(&[
        &[2, 1, 1],
        &der(0x31, &hash_algorithm),
        &content,
        &certs,
        &der(0x31, &info),
    ]);
    Ok(sequence(&[SIGNED_DATA, &der(0xa0, &signed)]))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    #[test]
    fn rejects_malformed_and_short_cms_without_touching_stores() {
        assert!(verify(&[0x30, 0x80], &[], &[], &[]).is_err());
        let mut large = vec![0; 16 * 1024 + 1];
        large[0..3].copy_from_slice(&[0x30, 0x81, 0]);
        assert!(verify(&large, &[], &[], &[]).is_err());
    }

    pub(in super::super) fn fixture() -> (Vec<Vec<u8>>, Vec<u8>, Vec<u8>, Vec<u8>) {
        use openssl::asn1::Asn1Time;
        use openssl::bn::BigNum;
        use openssl::hash::MessageDigest;
        use openssl::pkey::PKey;
        use openssl::rsa::Rsa;
        use openssl::sign::Signer;
        use openssl::x509::{X509, X509NameBuilder};

        fn wrap(tag: u8, body: &[u8]) -> Vec<u8> {
            let mut out = vec![tag];
            if body.len() < 128 {
                out.push(body.len() as u8);
            } else {
                let bytes = body.len().to_be_bytes();
                let start = bytes.iter().position(|byte| *byte != 0).unwrap();
                out.push(0x80 | (bytes.len() - start) as u8);
                out.extend_from_slice(&bytes[start..]);
            }
            out.extend_from_slice(body);
            out
        }
        fn seq(parts: &[&[u8]]) -> Vec<u8> {
            wrap(0x30, &parts.concat())
        }
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "Synthetic CMS signer")
            .unwrap();
        let name = name.build();
        let mut builder = X509::builder().unwrap();
        builder.set_version(2).unwrap();
        builder
            .set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
            .unwrap();
        builder.set_subject_name(&name).unwrap();
        builder.set_issuer_name(&name).unwrap();
        builder.set_pubkey(&key).unwrap();
        builder
            .set_not_before(&Asn1Time::from_unix(0).unwrap())
            .unwrap();
        builder
            .set_not_after(&Asn1Time::from_unix(2_000_000_000).unwrap())
            .unwrap();
        builder.sign(&key, MessageDigest::sha256()).unwrap();
        let cert = builder.build();
        let der = cert.to_der().unwrap();
        let chain = vec![der.clone(); 32];
        let attributes = [0x31, 0];
        let mut signer = Signer::new(MessageDigest::sha256(), &key).unwrap();
        signer.update(&attributes).unwrap();
        let signature = signer.sign_to_vec().unwrap();
        let sha = seq(&[
            &[6, 9, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01],
            &[5, 0],
        ]);
        let rsa = seq(&[
            &[6, 9, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01],
            &[5, 0],
        ]);
        let identity = seq(&[&cert.issuer_name().to_der().unwrap(), &[2, 1, 1]]);
        let info = seq(&[
            &[2, 1, 1],
            &identity,
            &sha,
            &[0xa0, 0],
            &rsa,
            &wrap(4, &signature),
        ]);
        let content = seq(&[&[6, 9, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x01]]);
        let signed = seq(&[
            &[2, 1, 1],
            &wrap(0x31, &sha),
            &content,
            &wrap(0xa0, &chain.concat()),
            &wrap(0x31, &info),
        ]);
        let cms = seq(&[
            &[6, 9, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02],
            &wrap(0xa0, &signed),
        ]);
        (chain, attributes.to_vec(), signature, cms)
    }

    #[test]
    fn binds_oversized_cms_to_the_actual_signature_and_identity() {
        let (chain, attributes, signature, cms) = fixture();
        assert!(cms.len() > 16 * 1024);
        assert!(verify(&cms, &chain, &attributes, &signature).is_ok());
        let mut wrong = signature.clone();
        wrong[0] ^= 1;
        assert!(verify(&cms, &chain, &attributes, &wrong).is_err());
        assert!(verify(&cms, &chain[..1], &attributes, &signature).is_err());
    }
}
