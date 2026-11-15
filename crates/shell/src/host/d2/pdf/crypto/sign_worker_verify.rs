//! Verification of a completed signing operation in a separate owned worker.
//! All request inputs except `candidate` come from the native request owner.
use chrono::{DateTime, Datelike, Utc};
use shell_contract::pdf_sign_options::SignOptions;

use super::{cms, pdf_lib, signing_revision_digest};

fn element<'a>(input: &mut &'a [u8], tag: u8) -> Result<&'a [u8], String> {
    if input.first() != Some(&tag) { return Err("invalid CMS element".into()); }
    let first = *input.get(1).ok_or("truncated CMS")?;
    let (header, length) = if first < 128 { (2usize, usize::from(first)) } else {
        let count = usize::from(first & 127);
        if count == 0 || count > std::mem::size_of::<usize>() { return Err("invalid CMS length".into()); }
        let octets = input.get(2..2 + count).ok_or("truncated CMS length")?;
        if octets[0] == 0 { return Err("noncanonical CMS length".into()); }
        let mut length = 0usize;
        for octet in octets { length = length.checked_mul(256).and_then(|v| v.checked_add(usize::from(*octet))).ok_or("CMS length overflow")?; }
        if length < 128 { return Err("noncanonical CMS length".into()); }
        (2 + count, length)
    };
    let end = header.checked_add(length).ok_or("CMS length overflow")?;
    let body = input.get(header..end).ok_or("truncated CMS")?;
    *input = &input[end..];
    Ok(body)
}

// This locates the one signature; cms::verify independently checks every
// enclosing field, chain, algorithm, authenticated attribute and signature.
fn signature(cms: &[u8]) -> Result<&[u8], String> {
    let mut input = cms;
    let mut root = element(&mut input, 0x30)?;
    element(&mut root, 0x06)?;
    let mut explicit = element(&mut root, 0xa0)?;
    let mut signed = element(&mut explicit, 0x30)?;
    element(&mut signed, 0x02)?;
    element(&mut signed, 0x31)?;
    element(&mut signed, 0x30)?;
    element(&mut signed, 0xa0)?;
    let mut signers = element(&mut signed, 0x31)?;
    let mut signer = element(&mut signers, 0x30)?;
    element(&mut signer, 0x02)?;
    element(&mut signer, 0x30)?;
    element(&mut signer, 0x30)?;
    element(&mut signer, 0xa0)?;
    element(&mut signer, 0x30)?;
    element(&mut signer, 0x04)
}

fn der_from_hex(hex: &[u8]) -> Result<Vec<u8>, String> {
    if hex.len() % 2 != 0 { return Err("invalid signature placeholder".into()); }
    fn nibble(byte: u8) -> Result<u8, String> {
        match byte { b'0'..=b'9' => Ok(byte-b'0'), b'A'..=b'F' => Ok(byte-b'A'+10), _ => Err("invalid signature hex".into()) }
    }
    let mut bytes = Vec::with_capacity(hex.len()/2);
    for pair in hex.chunks_exact(2) { bytes.push((nibble(pair[0])? << 4) | nibble(pair[1])?); }
    let mut rest = bytes.as_slice();
    element(&mut rest, 0x30)?;
    let length = bytes.len() - rest.len();
    if !rest.iter().all(|b| *b == 0) { return Err("nonzero signature padding".into()); }
    bytes.truncate(length);
    Ok(bytes)
}

fn times(time: DateTime<Utc>) -> Result<(String, Vec<u8>), String> {
    let pdf = time.format("D:%Y%m%d%H%M%SZ").to_string();
    let (tag, value) = if (1950..2050).contains(&time.year()) { (0x17, time.format("%y%m%d%H%M%SZ").to_string()) } else { (0x18, time.format("%Y%m%d%H%M%SZ").to_string()) };
    let mut der = vec![tag, u8::try_from(value.len()).map_err(|_| "invalid signing time")?];
    der.extend_from_slice(value.as_bytes());
    Ok((pdf, der))
}

/// Rebuild the entire authorized revision, then check the only permitted
/// change (the CMS placeholder) and authenticate it against the captured chain.
pub(super) fn verify_signed_document(original: &[u8], options: &SignOptions, signing_time: DateTime<Utc>, chain: &[Vec<u8>], candidate: &[u8]) -> Result<(), String> {
    if chain.is_empty() { return Err("missing signer certificate".into()); }
    let (pdf_time, der_time) = times(signing_time)?;
    let first = pdf_lib::assemble(original, options, &pdf_time, 16 * 1024)?;
    let (expected, attempt) = if first.file.len() == candidate.len() { (first, 1) } else {
        drop(first);
        (pdf_lib::assemble(original, options, &pdf_time, 48 * 1024)?, 2)
    };
    let [start, before, after, tail] = expected.range;
    if start != 0 || after.checked_add(tail) != Some(expected.file.len()) || expected.file.len() != candidate.len() || before.checked_add(2).is_none_or(|n| n > after) {
        return Err("invalid signed revision length".into());
    }
    if candidate[..=before] != expected.file[..=before] || candidate[after-1..] != expected.file[after-1..] {
        return Err("signed revision differs from native request".into());
    }
    let cms = der_from_hex(&candidate[before+1..after-1])?;
    if attempt == 2 && cms.len() <= 16 * 1024 { return Err("unnecessary signing retry".into()); }
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    hash.update(&expected.file[..before]);
    hash.update(&expected.file[after..]);
    let attrs = cms::attributes(hash.finish().as_ref(), &der_time);
    signing_revision_digest(original, &expected.file, expected.range, &attrs, &der_time, attempt)?;
    let signature = signature(&cms)?;
    cms::verify(&cms, chain, &attrs, signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::{asn1::Asn1Time, bn::BigNum, hash::MessageDigest, pkey::PKey, rsa::Rsa, x509::{X509NameBuilder, X509}};
    use super::super::session::Session;

    #[test]
    fn completed_document_is_bound_to_input_options_time_chain_and_bytes() {
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap(); name.append_entry_by_text("CN", "Synthetic verifier").unwrap(); let name = name.build();
        let mut cert = X509::builder().unwrap(); cert.set_version(2).unwrap(); cert.set_serial_number(&BigNum::from_u32(7).unwrap().to_asn1_integer().unwrap()).unwrap(); cert.set_subject_name(&name).unwrap(); cert.set_issuer_name(&name).unwrap(); cert.set_pubkey(&key).unwrap(); cert.set_not_before(&Asn1Time::from_unix(0).unwrap()).unwrap(); cert.set_not_after(&Asn1Time::from_unix(2_000_000_000).unwrap()).unwrap(); cert.sign(&key, MessageDigest::sha256()).unwrap();
        let chain = vec![cert.build().to_der().unwrap()];
        let retry_key = key.clone();
        let signer = Box::new(move |digest: &[u8; 32]| { let mut ctx = openssl::pkey_ctx::PkeyCtx::new(&key).map_err(|e| e.to_string())?; ctx.sign_init().map_err(|e| e.to_string())?; ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1).map_err(|e| e.to_string())?; ctx.set_signature_md(openssl::md::Md::sha256()).map_err(|e| e.to_string())?; let mut signature = Vec::new(); ctx.sign_to_vec(digest, &mut signature).map_err(|e| e.to_string())?; Ok(signature) });
        let original = pdf_lib::synthetic_classic_pdf(); let options = SignOptions::default(); let time = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let candidate = Session::new(original.clone(), options.clone(), time, chain.clone(), None, Box::new(|| Ok(())), Box::new(|_| Ok(())), signer).unwrap().sign().unwrap();
        assert!(verify_signed_document(&original, &options, time, &chain, &candidate).is_ok());
        let mut changed = candidate.clone(); changed[0] ^= 1; assert!(verify_signed_document(&original, &options, time, &chain, &changed).is_err());
        let mut changed = candidate.clone(); let index = changed.windows(10).position(|w| w == b"/Contents ").unwrap()+11; changed[index] = if changed[index] == b'0' { b'1' } else { b'0' }; assert!(verify_signed_document(&original, &options, time, &chain, &changed).is_err());
        let retry_chain = vec![chain[0].clone(); 32];
        let signer = Box::new(move |digest: &[u8; 32]| { let mut ctx = openssl::pkey_ctx::PkeyCtx::new(&retry_key).map_err(|e| e.to_string())?; ctx.sign_init().map_err(|e| e.to_string())?; ctx.set_rsa_padding(openssl::rsa::Padding::PKCS1).map_err(|e| e.to_string())?; ctx.set_signature_md(openssl::md::Md::sha256()).map_err(|e| e.to_string())?; let mut signature = Vec::new(); ctx.sign_to_vec(digest, &mut signature).map_err(|e| e.to_string())?; Ok(signature) });
        let retry = Session::new(original.clone(), options.clone(), time, retry_chain.clone(), None, Box::new(|| Ok(())), Box::new(|_| Ok(())), signer).unwrap().sign().unwrap();
        assert!(verify_signed_document(&original, &options, time, &retry_chain, &retry).is_ok());
        assert!(verify_signed_document(&original, &options, time, &chain, &retry).is_err());
        assert!(verify_signed_document(&original, &options, time + chrono::Duration::seconds(1), &chain, &candidate).is_err());
        assert!(verify_signed_document(&original, &options, time, &[], &candidate).is_err());
        let mut changed_options = options.clone(); changed_options.reason = Some("different".into()); assert!(verify_signed_document(&original, &changed_options, time, &chain, &candidate).is_err());
        let mut changed = candidate.clone(); changed.push(b' '); assert!(verify_signed_document(&original, &options, time, &chain, &changed).is_err());
    }
    #[test]
    fn rejects_bad_der_padding_and_noncanonical_lengths() {
        assert!(der_from_hex(b"30000000").is_ok());
        assert!(der_from_hex(b"30000001").is_err());
        assert!(der_from_hex(b"308100").is_err());
        assert!(der_from_hex(b"3000ff").is_err());
    }
}
