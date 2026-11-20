use std::path::Path;
use std::sync::Arc;

use super::{NativeDriveMoveReceipt, NativeRequestOwner, SavedPublication};
use crate::hosts::OwnedEffectsHost;
use crate::runtime::CancelToken;

/// A saved publication or a Drive service's exact derived cache path.
#[derive(Clone)]
pub enum NativeSavedPath {
    Selected(SavedPublication),
    DriveMoved(Arc<dyn NativeDriveMoveReceipt>),
    DriveFallback(Arc<dyn NativeDriveMoveReceipt>),
}

impl NativeSavedPath {
    pub fn publication(&self) -> &SavedPublication {
        match self {
            Self::Selected(publication) => publication,
            Self::DriveMoved(receipt) | Self::DriveFallback(receipt) => receipt.publication(),
        }
    }

    pub fn matches_publication(&self, publication: &SavedPublication) -> bool {
        self.publication().same_publication(publication)
    }

    pub fn same_witness(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Selected(a), Self::Selected(b)) => a.same_publication(b),
            (Self::DriveMoved(a), Self::DriveMoved(b))
            | (Self::DriveFallback(a), Self::DriveFallback(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Selected(publication) => publication.path(),
            Self::DriveMoved(receipt) => receipt.moved_path(),
            Self::DriveFallback(receipt) => receipt.fallback_path(),
        }
    }

    /// One canonical admission, including newly captured target owners. A
    /// derived path is admitted by its Drive receipt, never by a raw path grant.
    pub fn with_effect(
        &self,
        effects: &OwnedEffectsHost,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        match self {
            Self::Selected(publication) => {
                let extra: Vec<_> = additional.iter().collect();
                publication.with_effect(
                    effects.as_ref(),
                    participants,
                    cancel,
                    pending,
                    &extra,
                    effect,
                )
            }
            Self::DriveMoved(receipt) | Self::DriveFallback(receipt) => {
                let mut extra = Vec::with_capacity(additional.len() + 2);
                extra.push(cancel);
                extra.push(pending);
                extra.extend(additional);
                match self {
                    Self::DriveMoved(_) => {
                        receipt.with_moved_effect(participants, &extra, &mut |_| effect())
                    }
                    Self::DriveFallback(_) => {
                        receipt.with_fallback_effect(participants, &extra, &mut |_| effect())
                    }
                    Self::Selected(_) => unreachable!(),
                }
            }
        }
    }
}
