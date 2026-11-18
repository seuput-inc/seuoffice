//! Request-owned signing session phases. Authority admission is external and
//! must serialize with all native revocations before calling `admit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Opening,
    AwaitingSave,
    Signing { admitted: u8 },
    AwaitingVerification,
    Verified,
    Closed,
}

#[derive(Debug)]
pub(super) struct State { phase: Phase }
impl State {
    pub(super) fn new() -> Self { Self { phase: Phase::Opening } }
    pub(super) fn opened(&mut self) -> Result<(), String> {
        if self.phase != Phase::Opening { return Err("unexpected signer open".into()); }
        self.phase = Phase::AwaitingSave; Ok(())
    }
    pub(super) fn begin(&mut self) -> Result<(), String> {
        if self.phase != Phase::AwaitingSave { return Err("signer is not awaiting a document".into()); }
        self.phase = Phase::Signing { admitted: 0 }; Ok(())
    }
    /// Must run under the short native authority gate. Retry evidence has
    /// already been independently checked against the captured request.
    pub(super) fn admit(&mut self, retry_verified: bool) -> Result<(), String> {
        let next = match self.phase {
            Phase::Signing { admitted: 0 } if !retry_verified => 1,
            Phase::Signing { admitted: 1 } if retry_verified => 2,
            _ => return Err("invalid signing admission order".into()),
        };
        self.phase = Phase::Signing { admitted: next }; Ok(())
    }
    pub(super) fn signed(&mut self) -> Result<(), String> {
        if !matches!(self.phase, Phase::Signing { admitted: 1 | 2 }) { return Err("unexpected signed result".into()); }
        self.phase = Phase::AwaitingVerification; Ok(())
    }
    pub(super) fn verified(&mut self) -> Result<(), String> {
        if self.phase != Phase::AwaitingVerification { return Err("unexpected verifier result".into()); }
        self.phase = Phase::Verified; Ok(())
    }
    pub(super) fn finish(&mut self) -> Result<(), String> {
        if self.phase != Phase::Verified { return Err("signing result is not verified".into()); }
        self.phase = Phase::Closed; Ok(())
    }
    pub(super) fn revoke(&mut self) { self.phase = Phase::Closed; }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revoke_before_admission_denies_provider_and_result() {
        let mut state = State::new(); state.opened().unwrap(); state.begin().unwrap(); state.revoke();
        assert!(state.admit(false).is_err()); assert!(state.signed().is_err()); assert!(state.finish().is_err());
    }
    #[test]
    fn admission_before_revoke_does_not_authorize_completion() {
        let mut state = State::new(); state.opened().unwrap(); state.begin().unwrap(); state.admit(false).unwrap();
        state.revoke(); assert!(state.signed().is_err()); assert!(state.admit(true).is_err());
    }
    #[test]
    fn retry_is_single_use_and_requires_verified_evidence() {
        let mut state = State::new(); state.opened().unwrap(); state.begin().unwrap();
        assert!(state.admit(true).is_err()); state.admit(false).unwrap(); assert!(state.admit(false).is_err());
        state.admit(true).unwrap(); assert!(state.admit(true).is_err()); state.signed().unwrap();
        assert!(state.finish().is_err()); state.verified().unwrap(); state.finish().unwrap(); assert!(state.finish().is_err());
    }
}
