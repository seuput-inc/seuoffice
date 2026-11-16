use std::fs;
use std::path::{Path, PathBuf};

pub(super) struct Sandbox(PathBuf);
impl Sandbox {
    pub(super) fn new() -> Self {
        Self::in_env("SEUOFFICE_HOSTED_TRASH_TEST_ROOT")
    }
    pub(super) fn in_env(name: &str) -> Self {
        let parent = PathBuf::from(
            std::env::var_os(name)
                .unwrap_or_else(|| panic!("hosted-only fixture requires explicit {name}")),
        )
        .canonicalize()
        .expect("fixture parent must already exist");
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).expect("fixture entropy");
        let name = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = parent.join(format!("seuoffice-prepared-trash-fixture-{name}"));
        fs::create_dir(&path).expect("exclusive fixture root");
        Self(path)
    }
    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!(
                "hosted trash fixture retained at {}: {error}",
                self.0.display()
            );
        }
    }
}
