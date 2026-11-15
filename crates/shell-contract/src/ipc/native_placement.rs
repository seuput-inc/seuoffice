//! Identity evidence for one accepted native placement transition.
use std::sync::Arc;

#[derive(Clone)]
pub struct PlacementMoveIdentity(Arc<()>);

impl PlacementMoveIdentity {
    pub fn same_move(&self, other: &Self) -> bool { Arc::ptr_eq(&self.0, &other.0) }
}

#[derive(Clone)]
pub struct PlacementMoveIssuer {
    identity: PlacementMoveIdentity,
}

impl PlacementMoveIssuer {
    pub fn new() -> (Self, PlacementMoveIdentity) {
        let identity = PlacementMoveIdentity(Arc::new(()));
        (Self { identity: identity.clone() }, identity)
    }

    pub(crate) fn superseded(self, successor: PlacementMoveIdentity) -> Result<PlacementSuperseded, String> {
        if self.identity.same_move(&successor) {
            return Err("A placement move cannot supersede itself".into());
        }
        Ok(PlacementSuperseded { original: self.identity, successor })
    }
}

#[derive(Clone)]
pub struct PlacementSuperseded {
    original: PlacementMoveIdentity,
    successor: PlacementMoveIdentity,
}

impl PlacementSuperseded {
    pub fn matches(&self, original: &PlacementMoveIdentity) -> bool { self.original.same_move(original) }
    pub fn successor_matches(&self, successor: &PlacementMoveIdentity) -> bool { self.successor.same_move(successor) }
}

#[cfg(test)]
mod tests {
    use super::PlacementMoveIssuer;

    #[test]
    fn supersession_is_bound_to_both_exact_move_identities() {
        let (original_issuer, original) = PlacementMoveIssuer::new();
        let (_, successor) = PlacementMoveIssuer::new();
        let (_, unrelated) = PlacementMoveIssuer::new();
        let proof = original_issuer.superseded(successor.clone()).expect("distinct successor");
        assert!(proof.matches(&original));
        assert!(!proof.matches(&unrelated));
        assert!(proof.successor_matches(&successor));
        assert!(!proof.successor_matches(&unrelated));
    }

    #[test]
    fn move_cannot_supersede_itself() {
        let (issuer, original) = PlacementMoveIssuer::new();
        assert!(issuer.superseded(original).is_err());
    }
}
