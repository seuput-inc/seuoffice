#[path = "nss_manifest.rs"]
mod nss_manifest;

mod nss_build;
use std::{env, path::Path};



fn main() {
    println!("cargo:rustc-check-cfg=cfg(seuoffice_nss_bundle)");
    println!("cargo:rerun-if-env-changed=SEUOFFICE_NSS_BUNDLE_DIR");
    println!("cargo:rerun-if-changed=nss_manifest.rs");
    println!("cargo:rerun-if-changed=../../tools/nss-helper/contract.json");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        match env::var_os("SEUOFFICE_NSS_BUNDLE_DIR") {
            Some(dir) => nss_build::embed_nss(Path::new(&dir)).unwrap_or_else(|e| panic!("NSS bundle: {e}")),
            None if env::var("PROFILE").as_deref() == Ok("release") => {
                panic!("Linux release requires SEUOFFICE_NSS_BUNDLE_DIR with a real verified NSS artifact manifest")
            }
            None => {}
        }
    }
    tauri_build::build()
}
