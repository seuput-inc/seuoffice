use std::ffi::{c_char, c_int, c_void};

use foreign_types::{ForeignType, ForeignTypeRef};
use openssl::error::ErrorStack;
use openssl::nid::Nid;
use openssl::pkcs12::Pkcs12;
use openssl::pkcs7::Pkcs7Ref;
use openssl::pkey::{Id, PKey, Private};
use openssl::x509::X509;
use openssl_sys as ffi;

// OpenSSL 3.6.3, openssl-src 300.6.1+3.6.3, pkcs12.h.in and p12_kiss.c.
// The C stack owns the elements returned by unpack; get0 borrows, get1 owns.
enum SafeBag {}

unsafe extern "C" {
    fn PKCS12_mac_present(p12: *const ffi::PKCS12) -> c_int;
    fn PKCS12_verify_mac(p12: *mut ffi::PKCS12, pass: *const c_char, len: c_int) -> c_int;
    fn PKCS12_unpack_authsafes(p12: *const ffi::PKCS12) -> *mut ffi::OPENSSL_STACK;
    fn PKCS12_unpack_p7data(p7: *mut ffi::PKCS7) -> *mut ffi::OPENSSL_STACK;
    fn PKCS12_unpack_p7encdata(p7: *mut ffi::PKCS7, pass: *const c_char, len: c_int) -> *mut ffi::OPENSSL_STACK;
    fn PKCS12_SAFEBAG_get_nid(bag: *const SafeBag) -> c_int;
    fn PKCS12_SAFEBAG_get_bag_nid(bag: *const SafeBag) -> c_int;
    fn PKCS12_SAFEBAG_get0_p8inf(bag: *const SafeBag) -> *const ffi::PKCS8_PRIV_KEY_INFO;
    fn PKCS12_decrypt_skey(bag: *const SafeBag, pass: *const c_char, len: c_int) -> *mut ffi::PKCS8_PRIV_KEY_INFO;
    fn PKCS12_SAFEBAG_get1_cert(bag: *const SafeBag) -> *mut ffi::X509;
    fn PKCS12_SAFEBAG_free(bag: *mut SafeBag);
}

unsafe extern "C" fn free_p7(value: *mut c_void) {
    unsafe { ffi::PKCS7_free(value.cast()) }
}

unsafe extern "C" fn free_bag(value: *mut c_void) {
    unsafe { PKCS12_SAFEBAG_free(value.cast()) }
}

struct Stack {
    ptr: *mut ffi::OPENSSL_STACK,
    free_item: unsafe extern "C" fn(*mut c_void),
}

impl Stack {
    unsafe fn new(ptr: *mut ffi::OPENSSL_STACK, free_item: unsafe extern "C" fn(*mut c_void)) -> Result<Self, String> {
        if ptr.is_null() { return Err(openssl_error()); }
        Ok(Self { ptr, free_item })
    }

    fn len(&self) -> usize {
        unsafe { ffi::OPENSSL_sk_num(self.ptr).max(0) as usize }
    }

    fn at(&self, index: usize) -> *mut c_void {
        unsafe { ffi::OPENSSL_sk_value(self.ptr, index as c_int) }
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        unsafe { ffi::OPENSSL_sk_pop_free(self.ptr, Some(self.free_item)) }
    }
}

struct PrivateInfo(*mut ffi::PKCS8_PRIV_KEY_INFO);
impl Drop for PrivateInfo {
    fn drop(&mut self) { unsafe { ffi::PKCS8_PRIV_KEY_INFO_free(self.0) } }
}

fn openssl_error() -> String {
    let detail = ErrorStack::get().to_string().to_ascii_lowercase();
    if detail.contains("unsupported") || detail.contains("unknown") {
        "cert-unsupported-key".into()
    } else if detail.contains("password") || detail.contains("mac") || detail.contains("decrypt") || detail.contains("bad decrypt") {
        "cert-password".into()
    } else {
        "cert-invalid".into()
    }
}

pub(super) struct Identity {
    pub(super) key: PKey<Private>,
    pub(super) chain_der: Vec<Vec<u8>>,
}

impl Identity {
    pub(super) fn sign_digest(&self, digest: &[u8; 32]) -> Result<Vec<u8>, String> {
        let mut context = openssl::pkey_ctx::PkeyCtx::new(&self.key).map_err(|error| error.to_string())?;
        context.sign_init().map_err(|error| error.to_string())?;
        context.set_rsa_padding(openssl::rsa::Padding::PKCS1).map_err(|error| error.to_string())?;
        context.set_signature_md(openssl::md::Md::sha256()).map_err(|error| error.to_string())?;
        let mut signature = Vec::new();
        context.sign_to_vec(digest, &mut signature).map_err(|error| error.to_string())?;
        Ok(signature)
    }
}

/// Extract all top-level bags in forge's order; select the first matching RSA identity.
pub(super) fn open(bytes: &[u8], password: &str, wanted_sha1: Option<&str>) -> Result<Identity, String> {
    let p12 = Pkcs12::from_der(bytes).map_err(|_| "cert-invalid".to_owned())?;
    let pass = password.as_bytes();
    let len = c_int::try_from(pass.len()).map_err(|_| "cert-invalid")?;
    let pass_ptr = pass.as_ptr().cast::<c_char>();
    // A non-null pointer with length zero preserves the empty-password case.
    if unsafe { PKCS12_mac_present(p12.as_ptr()) } != 0
        && unsafe { PKCS12_verify_mac(p12.as_ptr(), pass_ptr, len) } != 1
    {
        return Err("cert-password".into());
    }
    let safes = unsafe { Stack::new(PKCS12_unpack_authsafes(p12.as_ptr()), free_p7)? };
    let mut shrouded = Vec::new();
    let mut plain = Vec::new();
    let mut certs = Vec::new();
    let mut key_bags = 0usize;
    for index in 0..safes.len() {
        let p7 = safes.at(index).cast::<ffi::PKCS7>();
        if p7.is_null() { return Err("cert-invalid".into()); }
        // SAFETY: the owning safes stack outlives this borrowed PKCS7 reference.
        let kind = unsafe { Pkcs7Ref::from_ptr(p7) }.type_().map(|oid| oid.nid());
        let bags = match kind {
            Some(Nid::PKCS7_DATA) => unsafe { PKCS12_unpack_p7data(p7) },
            Some(Nid::PKCS7_ENCRYPTED) => unsafe { PKCS12_unpack_p7encdata(p7, pass_ptr, len) },
            _ => return Err("cert-unsupported-key".into()),
        };
        let bags = unsafe { Stack::new(bags, free_bag)? };
        for bag_index in 0..bags.len() {
            let bag = bags.at(bag_index).cast::<SafeBag>();
            if bag.is_null() { return Err("cert-invalid".into()); }
            let nid = unsafe { PKCS12_SAFEBAG_get_nid(bag) };
            if nid == Nid::KEYBAG.as_raw() || nid == Nid::PKCS8SHROUDEDKEYBAG.as_raw() {
                key_bags += 1;
                let decrypted;
                let info = if nid == Nid::KEYBAG.as_raw() {
                    unsafe { PKCS12_SAFEBAG_get0_p8inf(bag) }
                } else {
                    let ptr = unsafe { PKCS12_decrypt_skey(bag, pass_ptr, len) };
                    if ptr.is_null() { return Err(openssl_error()); }
                    decrypted = PrivateInfo(ptr);
                    decrypted.0 as *const ffi::PKCS8_PRIV_KEY_INFO
                };
                if info.is_null() { continue; }
                let key = unsafe { ffi::EVP_PKCS82PKEY(info) };
                if !key.is_null() {
                    // SAFETY: EVP_PKCS82PKEY returns a new owned EVP_PKEY.
                    let key: PKey<Private> = unsafe { PKey::from_ptr(key) };
                    if key.id() == Id::RSA {
                        if nid == Nid::KEYBAG.as_raw() { plain.push(key); } else { shrouded.push(key); }
                    }
                }
            } else if nid == Nid::CERTBAG.as_raw() {
                if unsafe { PKCS12_SAFEBAG_get_bag_nid(bag) } != Nid::X509CERTIFICATE.as_raw() {
                    return Err("cert-unsupported-key".into());
                }
                let cert = unsafe { PKCS12_SAFEBAG_get1_cert(bag) };
                if !cert.is_null() {
                    // SAFETY: get1_cert returns a new owned X509.
                    certs.push(unsafe { X509::from_ptr(cert) });
                }
            } else {
                return Err("cert-unsupported-key".into());
            }
        }
    }
    shrouded.extend(plain);
    if shrouded.is_empty() {
        return Err(if key_bags == 0 { "cert-no-key" } else { "cert-unsupported-key" }.into());
    }
    if certs.is_empty() { return Err("cert-invalid".into()); }
    for (index, cert) in certs.iter().enumerate() {
        let der = cert.to_der().map_err(|_| "cert-invalid")?;
        if let Some(wanted) = wanted_sha1 {
            let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, &der);
            let hex: String = digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
            if hex != wanted { continue; }
        }
        let Ok(public) = cert.public_key() else { continue };
        let Some(key_index) = shrouded.iter().position(|key| key.id() == Id::RSA && key.public_eq(&public)) else { continue };
        let key = shrouded.remove(key_index);
        let mut chain_der = Vec::with_capacity(certs.len());
        chain_der.push(der);
        for (other_index, other) in certs.iter().enumerate() {
            if other_index != index { chain_der.push(other.to_der().map_err(|_| "cert-invalid")?); }
        }
        return Ok(Identity { key, chain_der });
    }
    Err("cert-unsupported-key".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::bn::BigNum;
    use openssl::hash::MessageDigest;
    use openssl::rsa::Rsa;
    use openssl::x509::X509NameBuilder;
    use std::ptr;

    unsafe extern "C" {
        fn PKCS12_add_key(bags: *mut *mut ffi::OPENSSL_STACK, key: *mut ffi::EVP_PKEY,
            usage: c_int, iter: c_int, nid: c_int, pass: *const c_char) -> *mut SafeBag;
        fn PKCS12_add_cert(bags: *mut *mut ffi::OPENSSL_STACK, cert: *mut ffi::X509) -> *mut SafeBag;
        fn PKCS12_add_safe(safes: *mut *mut ffi::OPENSSL_STACK, bags: *mut ffi::OPENSSL_STACK,
            nid: c_int, iter: c_int, pass: *const c_char) -> c_int;
        fn PKCS12_add_safes(safes: *mut ffi::OPENSSL_STACK, nid: c_int) -> *mut ffi::PKCS12;
    }

    fn identity(label: &str, serial: u32) -> (PKey<Private>, X509) {
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", label).unwrap();
        let name = name.build();
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_serial_number(&BigNum::from_u32(serial).unwrap().to_asn1_integer().unwrap()).unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(&Asn1Time::from_unix(0).unwrap()).unwrap();
        cert.set_not_after(&Asn1Time::from_unix(2_000_000_000).unwrap()).unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        (key, cert.build())
    }

    #[test]
    fn selects_matching_identity_in_certificate_order_and_rejects_wrong_password() {
        let (first_key, first_cert) = identity("first", 1);
        let (second_key, second_cert) = identity("second", 2);
        let pass = c"synthetic";
        let mut bags = ptr::null_mut();
        let mut safes = ptr::null_mut();
        // These builder APIs allocate the stacks and leave them owned by the caller.
        unsafe {
            assert!(!PKCS12_add_key(&mut bags, first_key.as_ptr(), 0, 2048, -1, pass.as_ptr()).is_null());
            assert!(!PKCS12_add_key(&mut bags, second_key.as_ptr(), 0, 2048,
                Nid::PBE_WITHSHA1AND3_KEY_TRIPLEDES_CBC.as_raw(), pass.as_ptr()).is_null());
            assert!(!PKCS12_add_cert(&mut bags, second_cert.as_ptr()).is_null());
            assert!(!PKCS12_add_cert(&mut bags, first_cert.as_ptr()).is_null());
            assert_eq!(PKCS12_add_safe(&mut safes, bags, -1, 0, ptr::null()), 1);
            let bags = Stack::new(bags, free_bag).unwrap();
            let safes = Stack::new(safes, free_p7).unwrap();
            let raw = PKCS12_add_safes(safes.ptr, 0);
            assert!(!raw.is_null());
            let p12 = Pkcs12::from_ptr(raw);
            assert_eq!(ffi::PKCS12_set_mac(p12.as_ptr(), pass.as_ptr(), 9, ptr::null_mut(), 0, 2048, ptr::null()), 1);
            let bytes = p12.to_der().unwrap();
            let opened = open(&bytes, "synthetic", None).unwrap();
            assert_eq!(opened.chain_der[0], second_cert.to_der().unwrap());
            assert!(opened.key.public_eq(&second_key));
            let hash = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, &first_cert.to_der().unwrap());
            let wanted: String = hash.as_ref().iter().map(|b| format!("{b:02x}")).collect();
            let selected = open(&bytes, "synthetic", Some(&wanted)).unwrap();
            assert_eq!(selected.chain_der[0], first_cert.to_der().unwrap());
            assert!(selected.key.public_eq(&first_key));
            assert!(matches!(open(&bytes, "wrong", None), Err(error) if error == "cert-password"));
            drop(bags);
            drop(safes);
        }
    }

    #[test]
    fn malformed_pkcs12_is_rejected() {
        assert!(matches!(open(b"not a certificate", "synthetic", None), Err(error) if error == "cert-invalid"));
    }
}
