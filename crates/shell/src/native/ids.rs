//! Rust-minted identities. A label, a window handle or a process id is never an identity:
//! the registry maps a label to a [`BrowserId`], and every id is a counter value that is
//! never reused in this process.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! id {
    ($(#[$meta:meta])* $name:ident, $counter:ident) => {
        static $counter: AtomicU64 = AtomicU64::new(1);

        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(u64);

        impl $name {
            /// `None` once the counter is exhausted; callers refuse new work then.
            pub(crate) fn mint() -> Option<Self> {
                $counter
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| next.checked_add(1))
                    .ok()
                    .map(Self)
            }

            /// The raw value, for logs and for the public document id the bridge checks.
            pub fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id!(
    /// One native browser (a webview) for its whole lifetime.
    BrowserId,
    NEXT_BROWSER
);
id!(
    /// One admitted main-frame navigation, or one shell-initiated load.
    NavigationId,
    NEXT_NAVIGATION
);
id!(
    /// One committed document. Minted when the navigation commits.
    DocumentId,
    NEXT_DOCUMENT
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_ids_are_distinct_and_increasing() {
        let first = BrowserId::mint().unwrap();
        let second = BrowserId::mint().unwrap();
        assert_ne!(first, second);
        assert!(second.get() > first.get());
    }
}
