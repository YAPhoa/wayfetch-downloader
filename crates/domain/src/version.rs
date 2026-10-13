//! Revisions and generations.
//!
//! Identity is stable; a [`Revision`] orders concurrent edits to one record;
//! a [`Generation`] invalidates stale work (architecture brief section 6.1).
//! Both start at 1 and only move forward.

use std::fmt;
use std::num::NonZeroU64;

/// A counter reached `u64::MAX` and cannot advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterExhausted;

impl fmt::Display for CounterExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("revision or generation counter exhausted")
    }
}

impl std::error::Error for CounterExhausted {}

macro_rules! define_counter {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// The value of a newly created record.
            pub const INITIAL: Self = Self(NonZeroU64::MIN);

            /// Restores a stored value, rejecting zero.
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

            /// The next value. Fails rather than wrapping.
            pub fn next(self) -> Result<Self, CounterExhausted> {
                self.0.checked_add(1).map(Self).ok_or(CounterExhausted)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

define_counter!(
    /// Optimistic-concurrency version of a record. Every accepted change to
    /// the record advances it; a command carrying an older revision conflicts.
    Revision
);
define_counter!(
    /// Execution generation of a job. Only the coordinator advances it, and
    /// work started under an older generation may not write, publish, or
    /// change state.
    Generation
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_one_and_advance() {
        assert_eq!(Generation::INITIAL.get(), 1);
        assert_eq!(Generation::INITIAL.next().unwrap().get(), 2);
        assert_eq!(Revision::from_u64(0), None);
    }

    #[test]
    fn counters_never_wrap() {
        let last = Revision::from_u64(u64::MAX).unwrap();
        assert_eq!(last.next(), Err(CounterExhausted));
    }
}
