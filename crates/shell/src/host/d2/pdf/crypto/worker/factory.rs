//! Request-bound session plumbing. The authority is supplied by native ingress;
//! this module neither creates one nor registers the signer with IPC.
use super::{
    persistent::Persistent,
    process::{self, Operation},
    state::State,
};
use shell_contract::pdf_sign_options::SignOptions;
use shell_contract::{IpcValue, ipc::NativeRequestOwner, runtime::CancelToken};
use shell_ipc_d2::pdf::editor::hosts::{
    CertificateOpenCode, CertificateOpenError, CertificateOpenMessage, CertificateSource,
    SignFailure, SigningSession,
};
use std::sync::{Arc, Mutex};

type Opened = (Box<dyn SigningSession>, Vec<IpcValue>, Vec<Vec<u8>>);

// The verifier emits nonce + empty Verified/Guard, or nonce + "sign-failed".
const VERIFY_RESPONSE_BOUND: usize = 32 + b"sign-failed".len();

/// Implementations must serialize `action` with every invalidation of the exact
/// captured native document and request. Never hold this gate over a provider,
/// child process, verifier, dialog, or other blocking operation.
pub(crate) trait Authority: Send + Sync {
    /// Compare immutable authenticated origin and actual document lease identity,
    /// never view/renderer alone; false if proof is absent. Must not lock gates:
    /// this is also called inside `with_admission`.
    fn same_document(&self, opened: &NativeRequestOwner, current: &NativeRequestOwner) -> bool;
    fn is_current(&self, owner: &NativeRequestOwner, cancel: &CancelToken) -> bool;
    fn with_admission(
        &self,
        owner: &NativeRequestOwner,
        cancel: &CancelToken,
        action: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
}
/// Runs on a dedicated interaction thread, without an authority lock. The
/// implementation must dismiss its native modal on either cancellation token.
pub(crate) trait ProtectedTokenInteraction: Send + Sync {
    fn confirm(
        &self,
        owner: &NativeRequestOwner,
        request_cancel: &CancelToken,
        entry_cancel: &CancelToken,
        token_name: &str,
    ) -> Result<bool, String>;
}

struct IsolatedSession {
    worker: Persistent,
    authority: Arc<dyn Authority>,
    opened_owner: NativeRequestOwner,
    entry_cancel: CancelToken,
    chain: Vec<Vec<u8>>,
}
pub(crate) fn open(
    source: &CertificateSource,
    owner: &NativeRequestOwner,
    cancel: &CancelToken,
    entry_cancel: &CancelToken,
    authority: Arc<dyn Authority>,
    interaction: Arc<dyn ProtectedTokenInteraction>,
) -> Result<Opened, CertificateOpenError> {
    let result = open_inner(
        source,
        owner,
        cancel,
        entry_cancel,
        authority.clone(),
        interaction,
    );
    result.map_err(|error| {
        classify_open_error(
            &error,
            cancel.is_cancelled()
                || !authority.is_current(owner, cancel)
                || matches!(
                    error.as_str(),
                    "signing request cancelled" | "signing request revoked"
                ),
        )
    })
}

fn classify_open_error(error: &str, revoked: bool) -> CertificateOpenError {
    if error == "open-regex-limit" {
        return CertificateOpenError::Message(CertificateOpenMessage::RegexLimit);
    }
    if let Some(code) = CertificateOpenCode::parse(error) {
        return CertificateOpenError::Code(code);
    }
    if matches!(
        error,
        "signing frame allocation failed" | "signing frame length overflow"
    ) {
        return CertificateOpenError::Message(CertificateOpenMessage::ResourceExhausted);
    }
    if revoked {
        CertificateOpenError::Message(CertificateOpenMessage::Revoked)
    } else {
        CertificateOpenError::Message(CertificateOpenMessage::WorkerUnavailable)
    }
}

fn open_inner(
    source: &CertificateSource,
    owner: &NativeRequestOwner,
    cancel: &CancelToken,
    entry_cancel: &CancelToken,
    authority: Arc<dyn Authority>,
    interaction: Arc<dyn ProtectedTokenInteraction>,
) -> Result<Opened, String> {
    let (nonce, frames) = super::open_frames(source)?;
    let guarded_authority = authority.clone();
    let guarded_owner = owner.clone();
    let guarded_entry = entry_cancel.clone();
    let guard = Arc::new(move || {
        if guarded_entry.is_cancelled()
            || !guarded_authority.is_current(&guarded_owner, &guarded_entry)
        {
            Err("signing request revoked".into())
        } else {
            Ok(())
        }
    });
    let acquire_authority = authority.clone();
    let acquire_owner = owner.clone();
    let acquire_cancel = entry_cancel.clone();
    let opening_cancel = cancel.clone();
    let acquire = Box::new(move |evidence: Option<&[u8]>| {
        if evidence.is_some() {
            return Err("invalid certificate admission".into());
        }
        acquire_authority.with_admission(&acquire_owner, &opening_cancel, &mut || {
            acquire_cancel
                .with_active(|| Ok(()))
                .unwrap_or_else(|| Err("signing request revoked".into()))
        })
    });
    let interaction_owner = owner.clone();
    let interaction_cancel = cancel.clone();
    let interaction_entry = entry_cancel.clone();
    let confirm = Arc::new(move |name: String| {
        interaction.confirm(
            &interaction_owner,
            &interaction_cancel,
            &interaction_entry,
            &name,
        )
    });
    let requires_acquire = matches!(source, CertificateSource::System { .. });
    let (worker, opened) = Persistent::open(
        frames,
        nonce,
        guard,
        cancel,
        entry_cancel,
        acquire,
        requires_acquire,
        confirm,
    )?;
    let (summaries, chain) = super::unpack_opened(&opened)?;
    if chain.is_empty() {
        return Err("cert-invalid".into());
    }
    let session = IsolatedSession {
        worker,
        authority,
        opened_owner: owner.clone(),
        entry_cancel: entry_cancel.clone(),
        chain: chain.clone(),
    };
    Ok((Box::new(session), summaries, chain))
}

impl SigningSession for IsolatedSession {
    fn is_current(&self, owner: &NativeRequestOwner, cancel: &CancelToken) -> bool {
        self.worker.is_alive()
            && !self.entry_cancel.is_cancelled()
            && !cancel.is_cancelled()
            && self.authority.same_document(&self.opened_owner, owner)
            && self.authority.is_current(owner, cancel)
    }
    fn revoke_token(&self) -> CancelToken {
        self.entry_cancel.clone()
    }
    fn sign(
        &mut self,
        bytes: &[u8],
        options: &SignOptions,
        chain: &[Vec<u8>],
        owner: &NativeRequestOwner,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>, SignFailure> {
        let result = self.sign_inner(bytes, options, chain, owner, cancel);
        if result.is_err() {
            self.entry_cancel.cancel();
        }
        result.map_err(super::super::signature_failure)
    }
}

impl IsolatedSession {
    fn sign_inner(
        &mut self,
        bytes: &[u8],
        options: &SignOptions,
        chain: &[Vec<u8>],
        owner: &NativeRequestOwner,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>, String> {
        if owner.pdf_revision().is_none() || !self.is_current(owner, cancel) || chain != self.chain
        {
            return Err("signing request revoked".into());
        }
        let time = chrono::Utc::now();
        let frames = super::sign_frames(bytes, options, time, &self.chain, self.worker.nonce())?;
        let state = Arc::new(Mutex::new(State::new()));
        {
            let mut state = state.lock().map_err(|_| "signing state unavailable")?;
            state.opened()?;
            state.begin()?;
        }
        let original = Arc::new(bytes.to_vec());
        let options = Arc::new(options.clone());
        let chain = Arc::new(self.chain.clone());
        let authority = self.authority.clone();
        let owner = owner.clone();
        let opened_owner = self.opened_owner.clone();
        let request_cancel = cancel.clone();
        let entry_cancel = self.entry_cancel.clone();
        let admission_state = state.clone();
        let admission_original = original.clone();
        let admission_options = options.clone();
        let admission_chain = chain.clone();
        let admission_authority = authority.clone();
        let admission_owner = owner.clone();
        let admission_opened_owner = opened_owner.clone();
        let admission_cancel = request_cancel.clone();
        let admission_entry = entry_cancel.clone();
        let admit = Box::new(move |evidence: Option<&[u8]>| {
            if admission_entry.is_cancelled() {
                return Err("signing request revoked".into());
            }
            if let Some(evidence) = evidence {
                let (nonce, frames) = super::verification_frames(
                    &admission_original,
                    evidence,
                    &admission_options,
                    time,
                    &admission_chain,
                    true,
                )?;
                process::run(
                    frames,
                    nonce,
                    VERIFY_RESPONSE_BOUND,
                    Operation::VerifySignedDocument,
                    &admission_cancel,
                    || {
                        if !admission_entry.is_cancelled()
                            && admission_authority.is_current(&admission_owner, &admission_cancel)
                        {
                            Ok(())
                        } else {
                            Err("signing request revoked".into())
                        }
                    },
                    |_| Err("unexpected verifier admission".into()),
                )?;
            }
            admission_authority.with_admission(&admission_owner, &admission_cancel, &mut || {
                if !admission_authority.same_document(&admission_opened_owner, &admission_owner) {
                    return Err("signing request revoked".into());
                }
                admission_entry
                    .with_active(|| {
                        admission_state
                            .lock()
                            .map_err(|_| "signing state unavailable")?
                            .admit(evidence.is_some())
                    })
                    .unwrap_or_else(|| Err("signing request revoked".into()))
            })
        });
        let signed = self.worker.sign(frames, admit, cancel)?;
        state
            .lock()
            .map_err(|_| "signing state unavailable")?
            .signed()?;
        let (nonce, frames) =
            super::verification_frames(&original, &signed, &options, time, &chain, false)?;
        process::run(
            frames,
            nonce,
            VERIFY_RESPONSE_BOUND,
            Operation::VerifySignedDocument,
            cancel,
            || {
                if !entry_cancel.is_cancelled() && authority.is_current(&owner, &request_cancel) {
                    Ok(())
                } else {
                    Err("signing request revoked".into())
                }
            },
            |_| Err("unexpected verifier admission".into()),
        )?;
        authority.with_admission(&owner, &request_cancel, &mut || {
            if !authority.same_document(&opened_owner, &owner) {
                return Err("signing request revoked".into());
            }
            entry_cancel
                .with_active(|| {
                    let mut state = state.lock().map_err(|_| "signing state unavailable")?;
                    state.verified()?;
                    state.finish()
                })
                .unwrap_or_else(|| Err("signing request revoked".into()))
        })?;
        Ok(signed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_errors_only_expose_closed_codes_and_messages() {
        assert_eq!(
            classify_open_error("cert-store-locked", false),
            CertificateOpenError::Code(CertificateOpenCode::StoreLocked)
        );
        assert_eq!(
            classify_open_error("unexpected secret-bearing child text", false),
            CertificateOpenError::Message(CertificateOpenMessage::WorkerUnavailable)
        );
        assert_eq!(
            classify_open_error("signing frame allocation failed", false),
            CertificateOpenError::Message(CertificateOpenMessage::ResourceExhausted)
        );
        assert_eq!(
            classify_open_error("signing request cancelled", true),
            CertificateOpenError::Message(CertificateOpenMessage::Revoked)
        );
    }
}
