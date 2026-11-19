//! The D2 hosts' contracts hand over a sealed save destination; the saved-file
//! publication wants the revision authority and the path it was sealed for.

use shell_contract::hosts::{OwnedEffectsHost, SavedFile};
use shell_contract::ipc::{
    NativeRequestOwner, NativeSaveDestination, NativeSaveRevisionAuthority,
    NativeSaveRevisionReceipt,
};
use shell_contract::runtime::CancelToken;
use std::path::PathBuf;

/// The destination is its own first revision: the path it names is the path the
/// publication must find again. A D2 document never renews its native revision,
/// so a receipt that would renew it is refused.
#[allow(clippy::too_many_arguments)]
pub(super) fn file_saved_existing_document(
    destination: NativeSaveDestination,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effects: OwnedEffectsHost,
    document_links: bool,
    done: Box<dyn FnOnce(Result<SavedFile<PathBuf>, String>)>,
) {
    let expected_path = destination.path().to_path_buf();
    let revision = NativeSaveRevisionAuthority::new(destination, Vec::new());
    crate::host::d1::shells::file_saved_existing_document(
        revision,
        expected_path,
        participants,
        cancel,
        pending,
        additional,
        effects,
        document_links,
        std::rc::Rc::new(|_: &NativeSaveRevisionReceipt| {
            Err("a saved document of this kind cannot accept a native revision".into())
        }),
        done,
    );
}
