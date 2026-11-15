//! Persistable descriptions of the exact two-file recovery publication.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairImage {
    pub copy_sha256: [u8; 32],
    pub metadata_sha256: Option<[u8; 32]>,
    pub metadata: Option<Vec<u8>>,
}

impl PairImage {
    pub fn from_bytes(copy: &[u8], metadata: Option<Vec<u8>>) -> Self {
        let metadata_sha256 = metadata.as_ref().map(|bytes| Sha256::digest(bytes).into());
        Self {
            copy_sha256: Sha256::digest(copy).into(),
            metadata_sha256,
            metadata,
        }
    }

    pub fn from_snapshot(
        snapshot: &super::RecoveryPinnedSnapshot,
    ) -> Result<Self, crate::node_fs::NodeError> {
        Ok(Self::from_bytes(
            &snapshot.bytes,
            snapshot.exact_metadata()?.map(<[u8]>::to_vec),
        ))
    }

    fn copy_matches(&self, copy: Option<&[u8]>) -> bool {
        copy.is_some_and(|copy| <[u8; 32]>::from(Sha256::digest(copy)) == self.copy_sha256)
    }

    fn metadata_matches(&self, metadata: Option<&[u8]>) -> bool {
        self.metadata.as_deref() == metadata
            && metadata.map(|bytes| <[u8; 32]>::from(Sha256::digest(bytes))) == self.metadata_sha256
    }
}

/// Written durably before either public file is changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairIntent {
    pub generation: String,
    pub previous: Option<PairImage>,
    pub next: PairImage,
}

/// Written durably only after both public files match the next image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairCommit {
    pub generation: String,
    pub image: PairImage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterruptedPair {
    /// The old pair is still intact, including an absent old copy.
    Previous,
    /// The new pair is complete, even if its commit marker was not published.
    Next,
    /// Metadata was published but the old copy remains. The exact old metadata
    /// must be conditionally restored before this pair may be used.
    RestorePreviousMetadata(Option<Vec<u8>>),
    /// An entry is neither the old nor the new image. Never overwrite it.
    Conflict,
}

impl PairIntent {
    pub fn classify(&self, copy: Option<&[u8]>, metadata: Option<&[u8]>) -> InterruptedPair {
        if self.next.copy_matches(copy) && self.next.metadata_matches(metadata) {
            return InterruptedPair::Next;
        }
        match &self.previous {
            Some(previous)
                if previous.copy_matches(copy) && previous.metadata_matches(metadata) =>
            {
                InterruptedPair::Previous
            }
            Some(previous)
                if previous.copy_matches(copy) && self.next.metadata_matches(metadata) =>
            {
                InterruptedPair::RestorePreviousMetadata(previous.metadata.clone())
            }
            None if copy.is_none() && metadata.is_none() => InterruptedPair::Previous,
            None if copy.is_none() && self.next.metadata_matches(metadata) => {
                InterruptedPair::RestorePreviousMetadata(None)
            }
            _ => InterruptedPair::Conflict,
        }
    }

    pub fn commit_if_matching(
        &self,
        copy: Option<&[u8]>,
        metadata: Option<&[u8]>,
    ) -> Option<PairCommit> {
        (self.classify(copy, metadata) == InterruptedPair::Next).then(|| PairCommit {
            generation: self.generation.clone(),
            image: self.next.clone(),
        })
    }
}

impl PairCommit {
    pub fn matches(&self, copy: Option<&[u8]>, metadata: Option<&[u8]>) -> bool {
        self.image.copy_matches(copy) && self.image.metadata_matches(metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_windows_never_accept_a_mixed_pair() {
        let intent = PairIntent {
            generation: "generation-2".into(),
            previous: Some(PairImage::from_bytes(b"old", Some(b"old meta".to_vec()))),
            next: PairImage::from_bytes(b"new", Some(b"new meta".to_vec())),
        };
        assert_eq!(
            intent.classify(Some(b"old"), Some(b"old meta")),
            InterruptedPair::Previous
        );
        assert_eq!(
            intent.classify(Some(b"old"), Some(b"new meta")),
            InterruptedPair::RestorePreviousMetadata(Some(b"old meta".to_vec()))
        );
        assert_eq!(
            intent.classify(Some(b"new"), Some(b"new meta")),
            InterruptedPair::Next
        );
        assert_eq!(
            intent.classify(Some(b"other"), Some(b"new meta")),
            InterruptedPair::Conflict
        );
        assert_eq!(
            intent.classify(Some(b"old"), Some(b"other")),
            InterruptedPair::Conflict
        );
        assert!(
            intent
                .commit_if_matching(Some(b"old"), Some(b"new meta"))
                .is_none()
        );
        assert!(
            intent
                .commit_if_matching(Some(b"new"), Some(b"new meta"))
                .unwrap()
                .matches(Some(b"new"), Some(b"new meta"))
        );
    }

    #[test]
    fn first_publication_crash_restores_absence() {
        let intent = PairIntent {
            generation: "first".into(),
            previous: None,
            next: PairImage::from_bytes(b"new", Some(b"new meta".to_vec())),
        };
        assert_eq!(intent.classify(None, None), InterruptedPair::Previous);
        assert_eq!(
            intent.classify(None, Some(b"new meta")),
            InterruptedPair::RestorePreviousMetadata(None)
        );
        assert_eq!(
            intent.classify(Some(b"new"), Some(b"new meta")),
            InterruptedPair::Next
        );
        assert_eq!(
            intent.classify(None, Some(b"foreign")),
            InterruptedPair::Conflict
        );
    }
}
