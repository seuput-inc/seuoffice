//! Publish signed bytes only while the captured request authority remains live.

use crate::{ipc::native_owner::NativeOwnerAuthority, views::NativeAssetAuthorization};
use main_core::node_fs::{NodeError, node_error2};
use main_core::packages::electron_utils::atomic_write::atomic_write_guarded;
use shell_contract::{ipc::NativeRequestOwner, runtime::CancelToken};

/// Admission uses only the identities retained by the authenticated request.
pub(super) struct NativeSigningAuthority;

impl NativeSigningAuthority {
    pub(super) fn new() -> Self {
        Self
    }
}

fn captured(
    owner: &NativeRequestOwner,
) -> Option<(&NativeOwnerAuthority, &NativeAssetAuthorization)> {
    let origin = owner.context.authority_ref::<NativeOwnerAuthority>()?;
    let asset = owner.context.downcast_ref::<NativeAssetAuthorization>()?;
    if asset.view != owner.view
        || asset.surface != shell_contract::Surface::Pdf
        || !asset.matches_native_context(&origin.origin)
        || owner.loss_token().is_none()
    {
        return None;
    }
    Some((origin, asset))
}

impl super::crypto::SigningAuthority for NativeSigningAuthority {
    fn owner_loss(&self, owner: &NativeRequestOwner) -> Option<CancelToken> {
        captured(owner)?;
        owner.pdf_document()?;
        owner.loss_token()
    }

    fn same_document(&self, opened: &NativeRequestOwner, current: &NativeRequestOwner) -> bool {
        let (Some((opened_origin, opened_asset)), Some((current_origin, current_asset))) =
            (captured(opened), captured(current))
        else {
            return false;
        };
        let (Some(opened_renderer), Some(current_renderer)) =
            (opened.context.renderer(), current.context.renderer())
        else {
            return false;
        };
        let (Some(opened_pdf), Some(current_pdf)) = (opened.pdf_document(), current.pdf_document())
        else {
            return false;
        };
        opened.view == current.view
            && opened_origin.origin == current_origin.origin
            && opened_asset.same_lease(current_asset)
            && opened_renderer.same_generation(current_renderer)
            && opened_pdf.same_document(current_pdf)
    }

    fn is_current(&self, owner: &NativeRequestOwner, cancel: &CancelToken) -> bool {
        self.with_admission(owner, cancel, &mut || Ok(())).is_ok()
    }

    fn with_admission(
        &self,
        owner: &NativeRequestOwner,
        cancel: &CancelToken,
        action: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let (_, asset) = captured(owner).ok_or("signing request revoked")?;
        let renderer = owner.context.renderer().ok_or("signing request revoked")?;
        let pdf = owner.pdf_document().ok_or("signing request revoked")?;
        let owner_loss = owner.loss_token().ok_or("signing request revoked")?;
        asset
            .with_live(|| {
                renderer
                    .with_active(|| {
                        pdf.with_live(|| {
                            let mut request =
                                || cancel.with_active(|| owner_loss.with_active(&mut *action));
                            match owner.pdf_revision() {
                                Some(revision) => {
                                    revision.with_live(&mut request).flatten().flatten()
                                }
                                None => request().flatten(),
                            }
                        })
                        .flatten()
                    })
                    .flatten()
            })
            .flatten()
            .ok_or_else(|| "signing request revoked".to_owned())?
    }
}

#[derive(Debug)]
struct CommitError(String);

impl From<NodeError> for CommitError {
    fn from(error: NodeError) -> Self {
        Self(error.message)
    }
}

pub(super) fn write_signed_pdf(
    target: &str,
    bytes: &[u8],
    owner: &NativeRequestOwner,
    cancel: &CancelToken,
    authority: &dyn super::crypto::SigningAuthority,
) -> Result<(), String> {
    if owner.pdf_revision().is_none() {
        return Err("signing request revoked".into());
    }
    atomic_write_guarded(target, bytes, |temporary, destination| {
        authority
            .with_admission(owner, cancel, &mut || {
                std::fs::rename(temporary, destination)
                    .map_err(|error| node_error2(&error, "rename", temporary, destination).message)
            })
            .map_err(CommitError)
    })
    .map_err(|error: CommitError| error.0)
}
