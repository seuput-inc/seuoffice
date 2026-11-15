use std::{env, fs, path::PathBuf};

fn main() {
    let root = PathBuf::from(env::var_os("PDF_PROOF_ROOT").expect("PDF_PROOF_ROOT is required"));
    let pdf = root.join("crates/shell/src/host/d2/pdf");
    let source = format!(r#"
extern crate self as shell_contract;
#[path = {options:?}] pub mod pdf_sign_options;
mod host {{ pub mod d2 {{ pub mod pdf {{
    #[path = {codec:?}] pub mod pdf_codec;
    pub mod crypto {{
        #[path = {parser:?}] pub mod pdf_lib;
        pub fn assemble(bytes: &[u8], opts: &shell_contract::pdf_sign_options::SignOptions, time: &str, placeholder: usize) -> Result<(Vec<u8>, [usize; 4]), String> {{
            let result = pdf_lib::assemble(bytes, opts, time, placeholder)?;
            pdf_lib::validate_revision(bytes, &result.file, opts, time, result.range)?;
            Ok((result.file, result.range))
        }}
        pub fn validate(original: &[u8], revised: &[u8], opts: &shell_contract::pdf_sign_options::SignOptions, time: &str, range: [usize; 4]) -> Result<(), String> {{
            pdf_lib::validate_revision(original, revised, opts, time, range)
        }}
    }}
    pub mod native_engine {{ pub mod signatures {{
        #[path = {consumer:?}] mod pdf;
        pub fn inspect(bytes: &[u8]) -> serde_json::Value {{
            let Some(context) = pdf::context(bytes) else {{ return serde_json::json!({{"fields": []}}); }};
            let fields: Vec<_> = pdf::fields(&context).iter().map(|field| serde_json::json!({{
                "name": field.name, "page": pdf::page_index(&context, field),
                "range": pdf::byte_range(&context, field.value), "reason": pdf::text(&context, field.value, b"Reason"),
                "location": pdf::text(&context, field.value, b"Location"),
                "contact": pdf::text(&context, field.value, b"ContactInfo"),
                "certified": pdf::certified(&context, field.value)
            }})).collect();
            serde_json::json!({{"fields": fields}})
        }}
    }} }}
}} }} }}
"#,
        codec = pdf.join("pdf_codec.rs"), parser = pdf.join("crypto/pdf_lib.rs"),
        consumer = pdf.join("native_engine/signatures/pdf.rs"), options = root.join("crates/shell-contract/src/pdf_sign_options.rs"));
    for path in [pdf.join("pdf_codec.rs"), pdf.join("pdf_codec"), pdf.join("crypto/pdf_lib.rs"), pdf.join("crypto/pdf_lib"), pdf.join("native_engine/signatures/pdf.rs")] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-changed={}", root.join("crates/shell-contract/src/pdf_sign_options.rs").display());
    fs::write(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("modules.rs"), source).unwrap();
}
