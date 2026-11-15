//! An OS-selected conversion destination, never a renderer-provided path grant.
use super::{RecoveryAuthoritySnapshot, RecoveryOpenTarget};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct RecoveryPreparedDestination {
    target: RecoveryOpenTarget,
    snapshot: RecoveryAuthoritySnapshot,
    path: PathBuf,
}

impl RecoveryPreparedDestination {
    /// The native save-dialog callback calls this only inside the captured
    /// recovery preparation admission. Never construct it from an IPC argument.
    pub fn from_native_save_dialog(
        target: RecoveryOpenTarget,
        snapshot: RecoveryAuthoritySnapshot,
        path: PathBuf,
    ) -> Result<Self, String> {
        if !snapshot.belongs_to(&target) || snapshot.registered.is_some() {
            return Err("The recovery preparation is no longer pending.".into());
        }
        if path.as_os_str().is_empty() {
            return Err("The save dialog did not select a destination.".into());
        }
        Ok(Self {
            target,
            snapshot,
            path,
        })
    }
    pub fn target(&self) -> &RecoveryOpenTarget {
        &self.target
    }
    pub fn snapshot(&self) -> &RecoveryAuthoritySnapshot {
        &self.snapshot
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Preserve the original Slides rule: a terminal case-insensitive .pptx
    /// stays unchanged; every other selected name gets the literal suffix.
    pub fn for_slides_pptx(mut self) -> Self {
        let bytes = self.path.as_os_str().as_encoded_bytes();
        let pptx = bytes.len() >= 5 && bytes[bytes.len() - 5..].eq_ignore_ascii_case(b".pptx");
        if !pptx {
            let mut path = self.path.into_os_string();
            path.push(".pptx");
            self.path = PathBuf::from(path);
        }
        self
    }
}

#[derive(Clone)]
pub struct RecoveryCommittedDestination {
    receipt: super::RecoverySeedReceipt,
    snapshot: RecoveryAuthoritySnapshot,
    path: PathBuf,
}

impl RecoveryCommittedDestination {
    /// The native save-dialog callback must hold the exact receipt and receiver admission.
    pub fn from_native_save_dialog(
        receipt: super::RecoverySeedReceipt,
        snapshot: RecoveryAuthoritySnapshot,
        path: PathBuf,
    ) -> Result<Self, String> {
        if !receipt.matches(receipt.target()) || !snapshot.belongs_to(receipt.target()) {
            return Err("The recovery seed is no longer available.".into());
        }
        if path.as_os_str().is_empty() {
            return Err("The save dialog did not select a destination.".into());
        }
        Ok(Self {
            receipt,
            snapshot,
            path,
        })
    }
    pub fn receipt(&self) -> &super::RecoverySeedReceipt {
        &self.receipt
    }
    pub fn snapshot(&self) -> &RecoveryAuthoritySnapshot {
        &self.snapshot
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn for_docs_docx(mut self) -> Self {
        let bytes = self.path.as_os_str().as_encoded_bytes();
        if bytes.len() < 5 || !bytes[bytes.len() - 5..].eq_ignore_ascii_case(b".docx") {
            let mut path = self.path.into_os_string();
            path.push(".docx");
            self.path = path.into();
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Surface, ViewId,
        ipc::{NativeInvocationIssuer, NativeInvocationSelection, RecoveryOpenContext},
        runtime::{CancelToken, RendererGeneration},
    };

    fn destination(path: &str) -> RecoveryPreparedDestination {
        let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
            NativeInvocationSelection::RecoveryBlank {
                surface: Surface::Slides,
                title: "Untitled".into(),
            },
            CancelToken::default(),
        );
        let created = RendererGeneration::new();
        let (_, closure, owner) =
            crate::ipc::NativeClosureIssuer::new_with_owner(ViewId(1), created.clone());
        let target = RecoveryOpenTarget::bind(
            RecoveryOpenContext::NativeStartup(operation),
            ViewId(1),
            created,
            closure,
            owner,
        )
        .unwrap();
        let snapshot = target.authority_snapshot().unwrap();
        RecoveryPreparedDestination::from_native_save_dialog(target, snapshot, PathBuf::from(path))
            .unwrap()
    }

    #[test]
    fn slides_suffix_matches_original_terminal_case_insensitive_rule() {
        for (chosen, expected) in [
            ("deck.PPTX", "deck.PPTX"),
            (".PPTX", ".PPTX"),
            ("deck.foo", "deck.foo.pptx"),
            ("deck.", "deck..pptx"),
            ("deck", "deck.pptx"),
        ] {
            assert_eq!(
                destination(chosen).for_slides_pptx().path(),
                Path::new(expected)
            );
        }
    }

    #[test]
    fn committed_selection_retains_exact_receipt_and_docx_suffix() {
        let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
            NativeInvocationSelection::RecoveryBlank {
                surface: Surface::Docs,
                title: "Untitled".into(),
            },
            CancelToken::default(),
        );
        let created = RendererGeneration::new();
        let (_, closure, owner) =
            crate::ipc::NativeClosureIssuer::new_with_owner(ViewId(2), created.clone());
        let target = RecoveryOpenTarget::bind(
            RecoveryOpenContext::NativeStartup(operation),
            ViewId(2),
            created,
            closure,
            owner,
        )
        .unwrap();
        let receipt = target.commit_seed_from_admitted(&mut || Ok(())).unwrap();
        let snapshot = target.authority_snapshot().unwrap();
        for (chosen, expected) in [
            ("letter.DOCX", "letter.DOCX"),
            ("letter.doc", "letter.doc.docx"),
            ("letter", "letter.docx"),
        ] {
            let destination = RecoveryCommittedDestination::from_native_save_dialog(
                receipt.clone(),
                snapshot.clone(),
                PathBuf::from(chosen),
            )
            .unwrap()
            .for_docs_docx();
            assert_eq!(destination.path(), Path::new(expected));
            assert!(destination.receipt().matches(&target));
        }
    }
}
