//! Byte-level port of officecrypto-tool 0.0.19's ECMA-376 Agile reader.

use std::collections::HashMap;

use openssl::cipher::Cipher;
use openssl::cipher_ctx::CipherCtx;
use openssl::hash::{DigestBytes, Hasher, MessageDigest};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use zeroize::{Zeroize, Zeroizing};

use super::{OfficeDecryptError, unsupported, wrong_password};

const KEY_VALUE: [u8; 8] = [0x14, 0x6e, 0x0b, 0xe7, 0xab, 0xac, 0xd0, 0xd6];
const VERIFIER_INPUT: [u8; 8] = [0xfe, 0xa7, 0xd2, 0x76, 0x3b, 0x4b, 0x9e, 0x79];
const VERIFIER_VALUE: [u8; 8] = [0xd7, 0xaa, 0x0f, 0x6d, 0x30, 0x61, 0x34, 0x4e];

type Result<T, E = OfficeDecryptError> = std::result::Result<T, E>;
type Attributes = HashMap<String, String>;

struct SecretDigest(DigestBytes);

impl std::ops::Deref for SecretDigest {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for SecretDigest {
    fn drop(&mut self) {
        self.0[..].zeroize();
    }
}

struct Encryptor {
    cipher: String,
    chaining: String,
    hash: String,
    salt: Vec<u8>,
}

struct Agile {
    package: Encryptor,
    package_block_size: usize,
    password: Encryptor,
    spin_count: u64,
    key_bits: f64,
    verifier_input: Vec<u8>,
    verifier_value: Vec<u8>,
    key_value: Vec<u8>,
}

fn attributes(start: &BytesStart<'_>) -> Result<Attributes> {
    let mut result = HashMap::new();
    for attribute in start.attributes() {
        let attribute = attribute.map_err(|e| unsupported(e.to_string()))?;
        let name =
            std::str::from_utf8(attribute.key.as_ref()).map_err(|e| unsupported(e.to_string()))?;
        let value = attribute
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|e| unsupported(e.to_string()))?;
        result.insert(name.to_owned(), value.into_owned());
    }
    Ok(result)
}

fn required<'a>(attributes: &'a Attributes, key: &str) -> Result<&'a str> {
    attributes
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| unsupported(format!("Missing {key}")))
}

fn decode(attributes: &Attributes, key: &str) -> Result<Vec<u8>> {
    Ok(jscompat::buffer_from_base64(required(attributes, key)?))
}

fn number(attributes: &Attributes, key: &str) -> f64 {
    attributes
        .get(key)
        .map_or(f64::NAN, |value| jscompat::to_number(value))
}

fn encryptor(attrs: &Attributes) -> Result<Encryptor> {
    Ok(Encryptor {
        cipher: required(attrs, "cipherAlgorithm")?.to_owned(),
        chaining: required(attrs, "cipherChaining")?.to_owned(),
        hash: required(attrs, "hashAlgorithm")?.to_owned(),
        salt: decode(attrs, "saltValue")?,
    })
}

fn parse(info: &[u8]) -> Result<Agile> {
    let xml = info
        .get(8..)
        .ok_or_else(|| unsupported("EncryptionInfo too short"))?;
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut stack = Vec::<Vec<u8>>::new();
    let mut package = None;
    let mut password = None;
    loop {
        match reader
            .read_event()
            .map_err(|e| unsupported(e.to_string()))?
        {
            Event::Start(start) => {
                let name = start.name().as_ref().to_vec();
                if name == b"keyData" && stack.len() == 1 && stack[0] == b"encryption" {
                    if package.is_some() {
                        return Err(unsupported("Multiple keyData elements"));
                    }
                    package = Some(attributes(&start)?);
                }
                if name == b"p:encryptedKey"
                    && stack.len() == 3
                    && stack[0] == b"encryption"
                    && stack[1] == b"keyEncryptors"
                    && stack[2] == b"keyEncryptor"
                {
                    if password.is_some() {
                        return Err(unsupported("Multiple password key encryptors"));
                    }
                    password = Some(attributes(&start)?);
                }
                stack.push(name);
            }
            Event::End(_) => {
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let package = package.ok_or_else(|| unsupported("Missing keyData"))?;
    let password = password.ok_or_else(|| unsupported("Missing password key encryptor"))?;
    let block = number(&package, "blockSize");
    if !block.is_finite() || block < 1.0 || block.fract() != 0.0 || block > usize::MAX as f64 {
        return Err(unsupported("Invalid blockSize"));
    }
    let bits = number(&password, "keyBits");
    if bits == f64::INFINITY || bits / 8.0 > usize::MAX as f64 {
        return Err(unsupported("Invalid keyBits"));
    }
    let spins = number(&password, "spinCount");
    let spin_count = if spins.is_nan() || spins <= 0.0 {
        0
    } else if !spins.is_finite() || spins > u64::MAX as f64 {
        return Err(unsupported("Invalid spinCount"));
    } else {
        spins.ceil() as u64
    };
    Ok(Agile {
        package: encryptor(&package)?,
        package_block_size: block as usize,
        password: encryptor(&password)?,
        spin_count,
        key_bits: bits,
        verifier_input: decode(&password, "encryptedVerifierHashInput")?,
        verifier_value: decode(&password, "encryptedVerifierHashValue")?,
        key_value: decode(&password, "encryptedKeyValue")?,
    })
}

fn hasher(algorithm: &str) -> Result<Hasher> {
    let algorithm =
        MessageDigest::from_name(algorithm).ok_or_else(|| unsupported("Unknown digest"))?;
    Hasher::new(algorithm).map_err(|e| unsupported(e.to_string()))
}

fn digest(hash: &mut Hasher, parts: &[&[u8]]) -> Result<SecretDigest> {
    for part in parts {
        hash.update(part).map_err(|e| unsupported(e.to_string()))?;
    }
    hash.finish()
        .map(SecretDigest)
        .map_err(|e| unsupported(e.to_string()))
}

fn key(
    final_hash: &[u8],
    hash: &mut Hasher,
    block: &[u8],
    size: usize,
) -> Result<Zeroizing<Vec<u8>>> {
    let digest = digest(hash, &[final_hash, block])?;
    let mut bytes = Zeroizing::new(Vec::new());
    bytes
        .try_reserve_exact(size)
        .map_err(|_| unsupported("Key allocation failed"))?;
    bytes.extend_from_slice(&digest[..digest.len().min(size)]);
    bytes.resize(size, 0x36);
    Ok(bytes)
}

fn resolve_cipher(params: &Encryptor, key_len: usize) -> Result<Cipher> {
    if params.chaining != "ChainingModeCBC" {
        return Err(unsupported(format!(
            "Unknown cipher chaining: {}",
            params.chaining
        )));
    }
    let bits = key_len
        .checked_mul(8)
        .ok_or_else(|| unsupported("Invalid key size"))?;
    let name = format!("{}-{bits}-cbc", params.cipher.to_lowercase());
    if name.as_bytes().contains(&0) {
        return Err(unsupported("Invalid cipher name"));
    }
    let cipher = Cipher::fetch(None, &name, None).map_err(|e| unsupported(e.to_string()))?;
    if key_len != cipher.key_length() {
        return Err(unsupported("Invalid key length"));
    }
    Ok(cipher)
}

fn decrypt_cbc(cipher: &Cipher, key: &[u8], iv: &[u8], input: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    if iv.len() != cipher.iv_length() {
        return Err(unsupported("Invalid IV length"));
    }
    let mut context = CipherCtx::new().map_err(|e| unsupported(e.to_string()))?;
    context
        .decrypt_init(Some(cipher), Some(key), (!iv.is_empty()).then_some(iv))
        .map_err(|e| unsupported(e.to_string()))?;
    context.set_padding(false);
    let block = cipher.block_size();
    let capacity = input
        .len()
        .checked_add(block)
        .ok_or_else(|| unsupported("Cipher output too large"))?;
    let mut output = Zeroizing::new(Vec::new());
    output
        .try_reserve_exact(capacity)
        .map_err(|_| unsupported("Cipher allocation failed"))?;
    output.resize(capacity, 0);
    let mut position = 0;
    for chunk in input.chunks(4096) {
        position += context
            .cipher_update(chunk, Some(&mut output[position..]))
            .map_err(|e| unsupported(e.to_string()))?;
    }
    position += context
        .cipher_final(&mut output[position..])
        .map_err(|e| unsupported(e.to_string()))?;
    output.truncate(position);
    Ok(output)
}

pub(super) fn decrypt(info: &[u8], package: &[u8], password: &str) -> Result<Vec<u8>> {
    let params = parse(info)?;
    let mut password_bytes = Zeroizing::new(Vec::new());
    password_bytes
        .try_reserve(password.len().saturating_mul(2))
        .map_err(|_| unsupported("Password allocation failed"))?;
    for unit in password.encode_utf16() {
        password_bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let mut password_hash = hasher(&params.password.hash)?;
    let mut final_hash = digest(
        &mut password_hash,
        &[&params.password.salt, &password_bytes],
    )?;
    let key_bytes = params.key_bits / 8.0;
    let key_size = if key_bytes.is_nan() {
        final_hash.len()
    } else if key_bytes < 0.0 {
        let from_end = (-key_bytes) as usize;
        if from_end == 0 {
            0
        } else {
            final_hash.len().saturating_sub(from_end)
        }
    } else {
        key_bytes as usize
    };
    let password_cipher = resolve_cipher(&params.password, key_size)?;
    if params.password.salt.len() != password_cipher.iv_length() {
        return Err(unsupported("Invalid IV length"));
    }
    for i in 0..params.spin_count {
        let i = u32::try_from(i).map_err(|_| unsupported("Spin counter exceeds uint32"))?;
        final_hash = digest(&mut password_hash, &[&i.to_le_bytes(), &final_hash])?;
    }
    let verifier_input_key = key(&final_hash, &mut password_hash, &VERIFIER_INPUT, key_size)?;
    let verifier_value_key = key(&final_hash, &mut password_hash, &VERIFIER_VALUE, key_size)?;
    let verifier_input = decrypt_cbc(
        &password_cipher,
        &verifier_input_key,
        &params.password.salt,
        &params.verifier_input,
    )?;
    let verifier_value = decrypt_cbc(
        &password_cipher,
        &verifier_value_key,
        &params.password.salt,
        &params.verifier_value,
    )?;
    let calculated = digest(&mut password_hash, &[&verifier_input])?;
    if calculated.len() > verifier_value.len() {
        return Err(wrong_password());
    }
    let padding = verifier_value[calculated.len()..]
        .iter()
        .fold(0u8, |acc, byte| acc | byte);
    if !(openssl::memcmp::eq(&calculated, &verifier_value[..calculated.len()])
        & openssl::memcmp::eq(&[padding], &[0]))
    {
        return Err(wrong_password());
    }
    let key_key = key(&final_hash, &mut password_hash, &KEY_VALUE, key_size)?;
    let package_key = decrypt_cbc(
        &password_cipher,
        &key_key,
        &params.password.salt,
        &params.key_value,
    )?;
    let package_cipher = resolve_cipher(&params.package, package_key.len())?;
    if params.package_block_size != package_cipher.iv_length() {
        return Err(unsupported("Invalid IV length"));
    }
    let mut package_hash = hasher(&params.package.hash)?;
    let prefix = package
        .get(..8)
        .ok_or_else(|| unsupported("EncryptedPackage too short"))?;
    let size = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
    let mut output = Zeroizing::new(Vec::new());
    let body_len = package.len() - 8;
    let block = params.package_block_size;
    let full_padding = (block - 4096 % block) % block;
    let tail = body_len % 4096;
    let tail_padding = if tail == 0 {
        0
    } else {
        (block - tail % block) % block
    };
    let capacity = (body_len / 4096)
        .checked_mul(full_padding)
        .and_then(|padding| body_len.checked_add(padding))
        .and_then(|length| length.checked_add(tail_padding))
        .ok_or_else(|| unsupported("Package too large"))?;
    output
        .try_reserve_exact(capacity)
        .map_err(|_| unsupported("Package allocation failed"))?;
    for (index, chunk) in package[8..].chunks(4096).enumerate() {
        let index =
            u32::try_from(index).map_err(|_| unsupported("Package segment exceeds uint32"))?;
        let mut iv = digest(
            &mut package_hash,
            &[&params.package.salt, &index.to_le_bytes()],
        )?
        .to_vec();
        if params.package_block_size > iv.len() {
            iv.try_reserve_exact(params.package_block_size - iv.len())
                .map_err(|_| unsupported("IV allocation failed"))?;
            iv.resize(params.package_block_size, 0x36);
        } else {
            iv.truncate(params.package_block_size);
        }
        let remainder = chunk.len() % params.package_block_size;
        let padded_len = if remainder == 0 {
            chunk.len()
        } else {
            chunk
                .len()
                .checked_add(params.package_block_size - remainder)
                .ok_or_else(|| unsupported("Package segment too large"))?
        };
        let mut padded = Vec::new();
        padded
            .try_reserve_exact(padded_len)
            .map_err(|_| unsupported("Package allocation failed"))?;
        padded.extend_from_slice(chunk);
        padded.resize(padded_len, 0);
        let plain = decrypt_cbc(&package_cipher, &package_key, &iv, &padded)?;
        output
            .try_reserve(plain.len())
            .map_err(|_| unsupported("Package allocation failed"))?;
        output.extend_from_slice(&plain);
    }
    if size < output.len() {
        output[size..].zeroize();
        output.truncate(size);
    }
    if !output.starts_with(b"PK\x03\x04") {
        return Err(wrong_password());
    }
    Ok(std::mem::take(&mut *output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    fn encrypt(cipher: &Cipher, key: &[u8], iv: &[u8], input: &[u8]) -> Vec<u8> {
        let mut context = CipherCtx::new().unwrap();
        context
            .encrypt_init(Some(cipher), Some(key), Some(iv))
            .unwrap();
        context.set_padding(false);
        let mut output = vec![0; input.len() + cipher.block_size()];
        let mut len = context.cipher_update(input, Some(&mut output)).unwrap();
        len += context.cipher_final(&mut output[len..]).unwrap();
        output.truncate(len);
        output
    }

    fn encrypted_fixture(cipher_name: &str, hash_name: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let salt = [7u8; 16];
        let verifier = [42u8; 16];
        let package_key = [99u8; 32];
        let plaintext = b"PK\x03\x04synthetic Agile payload".to_vec();
        let mut hash = hasher(hash_name).unwrap();
        let password: Vec<_> = "pw".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let first = digest(&mut hash, &[&salt, &password]).unwrap();
        let final_hash = digest(&mut hash, &[&0u32.to_le_bytes(), &first]).unwrap();
        let input_key = key(&final_hash, &mut hash, &VERIFIER_INPUT, 32).unwrap();
        let value_key = key(&final_hash, &mut hash, &VERIFIER_VALUE, 32).unwrap();
        let key_key = key(&final_hash, &mut hash, &KEY_VALUE, 32).unwrap();
        let cipher = Cipher::fetch(
            None,
            &format!("{}-256-cbc", cipher_name.to_lowercase()),
            None,
        )
        .unwrap();
        let encrypted_input = encrypt(&cipher, &input_key, &salt, &verifier);
        let verifier_hash = digest(&mut hash, &[&verifier]).unwrap();
        let mut padded_hash = verifier_hash.to_vec();
        while padded_hash.len() % 16 != 0 {
            padded_hash.push(0);
        }
        let encrypted_value = encrypt(&cipher, &value_key, &salt, &padded_hash);
        let encrypted_key = encrypt(&cipher, &key_key, &salt, &package_key);
        let iv = digest(&mut hash, &[&salt, &0u32.to_le_bytes()]).unwrap();
        let mut padded = plaintext.clone();
        while padded.len() % 16 != 0 {
            padded.push(0);
        }
        let mut package = (plaintext.len() as u64).to_le_bytes().to_vec();
        package.extend(encrypt(&cipher, &package_key, &iv[..16], &padded));
        let xml = format!(
            r#"<encryption><keyData cipherAlgorithm="{cipher_name}" cipherChaining="ChainingModeCBC" hashAlgorithm="{hash_name}" blockSize="16" saltValue="{}"/><dataIntegrity encryptedHmacKey="!"/><keyEncryptors><keyEncryptor><p:encryptedKey cipherAlgorithm="{cipher_name}" cipherChaining="ChainingModeCBC" hashAlgorithm="{hash_name}" spinCount="1" keyBits="256" saltValue="{}" encryptedVerifierHashInput="{}" encryptedVerifierHashValue="{}" encryptedKeyValue="{}"/></keyEncryptor></keyEncryptors></encryption>"#,
            STANDARD.encode(salt),
            STANDARD.encode(salt),
            STANDARD.encode(encrypted_input),
            STANDARD.encode(encrypted_value),
            STANDARD.encode(encrypted_key),
        );
        let mut info = vec![4, 0, 4, 0, 0x40, 0, 0, 0];
        info.extend_from_slice(xml.as_bytes());
        (info, package, plaintext)
    }

    #[test]
    fn decrypts_dynamic_ciphers_and_digests() {
        for (cipher, hash) in [("CAMELLIA", "SHA3-256"), ("ARIA", "MD5")] {
            let (info, package, plain) = encrypted_fixture(cipher, hash);
            assert_eq!(decrypt(&info, &package, "pw").unwrap(), plain);
            assert_eq!(
                decrypt(&info, &package, "wrong").unwrap_err().reason,
                super::super::OfficeDecryptReason::WrongPassword
            );
        }
    }

    #[test]
    fn invalid_cipher_and_xml_do_not_panic() {
        let (info, package, _) = encrypted_fixture("ARIA", "SHA256");
        let mut bad = info.clone();
        bad.truncate(20);
        assert!(decrypt(&bad, &package, "pw").is_err());
        let params = Encryptor {
            cipher: "a\0es".into(),
            chaining: "ChainingModeCBC".into(),
            hash: "SHA256".into(),
            salt: vec![0; 16],
        };
        assert!(resolve_cipher(&params, 32).is_err());
    }
}

/// officecrypto-tool's standard encryption path always uses AES-128-ECB and SHA-1.
pub(super) fn decrypt_standard(info: &[u8], package: &[u8], password: &str) -> Result<Vec<u8>> {
    let word =
        |offset| super::u32_at(info, offset).ok_or_else(|| unsupported("EncryptionInfo too short"));
    if word(4)? & 0x3f != 0x24 {
        return Err(unsupported("EncryptionInfo mismatch"));
    }
    let header_len = usize::try_from(word(8)?).map_err(|_| unsupported("Invalid header length"))?;
    let end = 12usize
        .checked_add(header_len)
        .ok_or_else(|| unsupported("Invalid header length"))?;
    let header = info
        .get(12..end)
        .ok_or_else(|| unsupported("EncryptionHeader truncated"))?;
    let header_word = |offset| {
        super::u32_at(header, offset).ok_or_else(|| unsupported("EncryptionHeader truncated"))
    };
    let flags = header_word(0)? & 0x3f;
    let algorithm = header_word(8)?;
    let valid = match algorithm {
        0x660e | 0x660f | 0x6610 => flags == 0x24,
        0x6801 => flags == 0x04,
        0 => matches!(flags, 0x10 | 0x04 | 0x24),
        _ => false,
    };
    if !valid {
        return Err(unsupported("Encryption Flags/AlgID mismatch"));
    }
    if header_word(16)? / 8 != 16 {
        return Err(unsupported("Invalid key length"));
    }
    let verifier = info
        .get(end..)
        .ok_or_else(|| unsupported("EncryptionVerifier missing"))?;
    let salt = verifier
        .get(4..20)
        .ok_or_else(|| unsupported("EncryptionVerifier truncated"))?;
    let encrypted_verifier = verifier
        .get(20..36)
        .ok_or_else(|| unsupported("EncryptionVerifier truncated"))?;
    let encrypted_hash = verifier
        .get(40..)
        .ok_or_else(|| unsupported("EncryptionVerifier truncated"))?;

    let mut password_bytes = Zeroizing::new(Vec::new());
    password_bytes
        .try_reserve(password.len().saturating_mul(2))
        .map_err(|_| unsupported("Password allocation failed"))?;
    for unit in password.encode_utf16() {
        password_bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let mut hash = hasher("sha1")?;
    let mut state = digest(&mut hash, &[salt, &password_bytes])?;
    for index in 0u32..50_000 {
        state = digest(&mut hash, &[&index.to_le_bytes(), &state])?;
    }
    let final_hash = digest(&mut hash, &[&state, &0u32.to_le_bytes()])?;
    let mut pad = Zeroizing::new([0x36u8; 64]);
    for (index, value) in final_hash.iter().enumerate() {
        pad[index] ^= *value;
    }
    let derived = digest(&mut hash, &[&pad[..]])?;
    let key = &derived[..16];
    let cipher =
        Cipher::fetch(None, "aes-128-ecb", None).map_err(|e| unsupported(e.to_string()))?;
    let verifier_plain = decrypt_cbc(&cipher, key, &[], encrypted_verifier)?;
    let verifier_hash = decrypt_cbc(&cipher, key, &[], encrypted_hash)?;
    let expected = digest(&mut hash, &[&verifier_plain])?;
    if verifier_hash.len() < expected.len()
        || !openssl::memcmp::eq(&expected, &verifier_hash[..expected.len()])
    {
        return Err(wrong_password());
    }

    let prefix = package
        .get(..8)
        .ok_or_else(|| unsupported("EncryptedPackage too short"))?;
    let size = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
    let mut output = Zeroizing::new(Vec::new());
    let capacity = package
        .len()
        .checked_add(15)
        .ok_or_else(|| unsupported("Package too large"))?;
    output
        .try_reserve_exact(capacity)
        .map_err(|_| unsupported("Package allocation failed"))?;
    for chunk in package[8..].chunks(4096) {
        let padded_len = chunk
            .len()
            .checked_add(15)
            .ok_or_else(|| unsupported("Package too large"))?
            / 16
            * 16;
        let mut padded = vec![0u8; padded_len];
        padded[..chunk.len()].copy_from_slice(chunk);
        let plain = decrypt_cbc(&cipher, key, &[], &padded)?;
        output.extend_from_slice(&plain);
    }
    if size < output.len() {
        output[size..].zeroize();
        output.truncate(size);
    }
    Ok(std::mem::take(&mut *output))
}
