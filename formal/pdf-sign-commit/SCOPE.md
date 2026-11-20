# PDF signature commit boundary

The model covers one staged output and one target. `Acquire` represents the shared authorization and cancellation exclusion held for the final rename. `Revoke` and `Cancel` cannot cross that boundary. `ReplaceFailure` leaves the old target in place. The model does not yet establish that CEF context, browser and asset-lease revocations use the same boundary. That integration is required before the model corresponds to the complete signing implementation.

`PreservesTarget` states that refusal or replacement failure does not change the target. `SuccessOnlyAfterCommit` states that success is reported exactly when the signed replacement lands. TLC was not run in this worktree.
