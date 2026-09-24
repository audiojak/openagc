//! Identifier newtypes. Every id crossing a crate boundary is one of these,
//! never a bare string (spec §4.2).

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

string_id!(
    /// A local account's UUID.
    AccountId
);
string_id!(
    /// The provider's thread id (Gmail `threadId`). Stable across local
    /// database rebuilds; unique within an account.
    ThreadId
);
string_id!(
    /// The provider's message id (Gmail message `id`).
    MessageId
);
string_id!(
    /// The provider's label id: system ids like `INBOX`, or `Label_12`.
    LabelId
);
string_id!(
    /// A local attachment id (the store's rowid as a string).
    AttachmentId
);
string_id!(
    /// A local draft id (the store's rowid as a string).
    DraftId
);

/// Milliseconds since the Unix epoch, UTC.
pub type Millis = i64;
