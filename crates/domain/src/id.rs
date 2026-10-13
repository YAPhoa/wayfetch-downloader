//! Opaque, typed record identifiers.
//!
//! Identifiers are allocated by the store and never reused. They are distinct
//! types so a job ID cannot be passed where an account ID is expected. Their
//! text form is `<prefix>-<number>`, for example `job-42`.

use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

/// The text form of an identifier was malformed or had the wrong prefix.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("expected an identifier of the form `{expected_prefix}-<positive integer>`")]
pub struct ParseIdError {
    expected_prefix: &'static str,
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[cfg_attr(
            feature = "serde",
            derive(serde::Serialize, serde::Deserialize),
            serde(try_from = "String", into = "String")
        )]
        pub struct $name(NonZeroU64);

        impl $name {
            /// The prefix used in this identifier's text form.
            pub const PREFIX: &'static str = $prefix;

            /// Wraps a store-allocated value. Zero is never a valid identifier.
            pub const fn new(value: NonZeroU64) -> Self {
                Self(value)
            }

            /// Wraps a store-allocated value, rejecting zero.
            pub const fn from_u64(value: u64) -> Option<Self> {
                match NonZeroU64::new(value) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            /// The raw value, for storage.
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}-{}", Self::PREFIX, self.0)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let err = ParseIdError { expected_prefix: Self::PREFIX };
                let digits = s
                    .strip_prefix(Self::PREFIX)
                    .and_then(|rest| rest.strip_prefix('-'))
                    .ok_or_else(|| err.clone())?;
                // Reject signs, whitespace, and leading zeros so each ID has
                // exactly one text form.
                if digits.is_empty()
                    || digits.starts_with('0')
                    || !digits.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(err);
                }
                digits
                    .parse::<NonZeroU64>()
                    .map(Self)
                    .map_err(|_| err)
            }
        }

        impl TryFrom<String> for $name {
            type Error = ParseIdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                value.parse()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.to_string()
            }
        }
    };
}

define_id!(
    /// An input batch submitted by a user or client.
    BatchId,
    "batch"
);
define_id!(
    /// A source link discovered from an input batch or crawl.
    SourceLinkId,
    "src"
);
define_id!(
    /// A canonical resource identity (see [`crate::source::ResourceIdentity`]).
    ResourceId,
    "res"
);
define_id!(
    /// A package grouping file jobs.
    PackageId,
    "pkg"
);
define_id!(
    /// A file job.
    JobId,
    "job"
);
define_id!(
    /// A configured provider account.
    AccountId,
    "acct"
);
define_id!(
    /// A configured egress route.
    RouteId,
    "route"
);
define_id!(
    /// One execution attempt of a job generation.
    AttemptId,
    "att"
);
define_id!(
    /// A published artifact.
    ArtifactId,
    "art"
);
define_id!(
    /// A versioned group of archive volumes.
    ArchiveSetId,
    "aset"
);
define_id!(
    /// A configured download or extraction root.
    RootId,
    "root"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_form_round_trips() {
        let id = JobId::from_u64(42).unwrap();
        assert_eq!(id.to_string(), "job-42");
        assert_eq!("job-42".parse::<JobId>(), Ok(id));
    }

    #[test]
    fn zero_is_not_an_id() {
        assert_eq!(JobId::from_u64(0), None);
        assert!("job-0".parse::<JobId>().is_err());
    }

    #[test]
    fn rejects_wrong_prefix_and_noncanonical_text() {
        for text in [
            "pkg-1",
            "job1",
            "job-",
            "job-01",
            "job-+1",
            "job--1",
            "job- 1",
            "job-1 ",
            "JOB-1",
            "job-18446744073709551616",
        ] {
            assert!(text.parse::<JobId>().is_err(), "{text:?} parsed");
        }
    }

    #[test]
    fn largest_value_round_trips() {
        let id = AccountId::from_u64(u64::MAX).unwrap();
        assert_eq!(id.to_string().parse::<AccountId>(), Ok(id));
    }
}
