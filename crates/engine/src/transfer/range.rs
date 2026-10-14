//! Checking a response against the byte range that was requested (section
//! 7.5, ADR-14; tests T02 to T04 and T08).
//!
//! The HTTP layer parses status and headers; this module decides what the
//! parsed framing allows, so the rules hold whichever client produced it.

use std::ops::Range;

/// How a response frames its body, parsed from status and headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// `200`: the whole representation, of `length` bytes if known.
    Full {
        /// `Content-Length`, if sent.
        length: Option<u64>,
    },
    /// `206` with one `Content-Range: bytes first-last/complete`.
    Partial {
        /// First byte, inclusive.
        first: u64,
        /// Last byte, inclusive.
        last: u64,
        /// Complete length, or `None` for `*`.
        complete: Option<u64>,
    },
    /// `416` with `Content-Range: bytes */complete`, if sent.
    Unsatisfiable {
        /// Complete length, if sent.
        complete: Option<u64>,
    },
}

/// What to do with a response's body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Write the body from `start`. With a known end, the body must be
    /// exactly that long: fewer bytes are a truncation, more an
    /// overflow (rule 7.5.7).
    Write {
        /// Where the body goes; an open end means "until the body ends".
        start: u64,
        /// Exclusive end, if known.
        end: Option<u64>,
        /// The representation's complete length, if known.
        complete: Option<u64>,
    },
    /// The server ignored the range and sent the whole representation. It
    /// must never be appended to the partial; restart from zero in a new
    /// generation or fail, as the offer's policy says (rule 7.5.2, T03).
    RestartFromZero {
        /// The representation's length, if known.
        length: Option<u64>,
    },
    /// `416` is not completion: compare durable coverage with the complete
    /// length, and finish only with [`is_complete`] (rule 7.5.3, T04).
    Reconcile {
        /// Complete length, if the server sent it.
        complete: Option<u64>,
    },
}

/// Why a response cannot be used. Its bytes are never written; which
/// failure that records depends on the provider and is the HTTP layer's
/// call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RangeViolation {
    /// A `206` or `416` answered a request without a range.
    #[error("range response to a request without a range")]
    Unrequested,
    /// The `Content-Range` is malformed: an inverted range or one past the
    /// complete length.
    #[error("invalid content range")]
    Invalid,
    /// The returned range does not start where the request did.
    #[error("response starts at {found}, requested {requested}")]
    WrongStart {
        /// The requested first byte.
        requested: u64,
        /// The returned first byte.
        found: u64,
    },
    /// The returned range extends past the requested one.
    #[error("response extends past the requested range")]
    BeyondRequest,
    /// The complete length differs from the one already known.
    #[error("complete length changed from {expected} to {found}")]
    LengthChanged {
        /// The known length.
        expected: u64,
        /// The reported length.
        found: u64,
    },
}

/// Decides what a response to `requested` allows, given the complete
/// length already `known` for this representation. `requested` is `None`
/// for a request without a `Range` header, else the half-open byte
/// interval asked for, with an open end for `bytes=start-`.
pub fn plan(
    requested: Option<(u64, Option<u64>)>,
    framing: Framing,
    known: Option<u64>,
) -> Result<Plan, RangeViolation> {
    let check_length = |found: Option<u64>| match (known, found) {
        (Some(expected), Some(found)) if expected != found => {
            Err(RangeViolation::LengthChanged { expected, found })
        }
        _ => Ok(()),
    };
    match (requested, framing) {
        (None, Framing::Full { length }) | (Some((0, None)), Framing::Full { length }) => {
            check_length(length)?;
            Ok(Plan::Write {
                start: 0,
                end: length,
                complete: length,
            })
        }
        (Some(_), Framing::Full { length }) => {
            check_length(length)?;
            Ok(Plan::RestartFromZero { length })
        }
        (None, Framing::Partial { .. } | Framing::Unsatisfiable { .. }) => {
            Err(RangeViolation::Unrequested)
        }
        (
            Some((start, end)),
            Framing::Partial {
                first,
                last,
                complete,
            },
        ) => {
            let stop = last.checked_add(1).ok_or(RangeViolation::Invalid)?;
            if last < first || complete.is_some_and(|c| stop > c) {
                return Err(RangeViolation::Invalid);
            }
            if first != start {
                return Err(RangeViolation::WrongStart {
                    requested: start,
                    found: first,
                });
            }
            if end.is_some_and(|end| stop > end) {
                return Err(RangeViolation::BeyondRequest);
            }
            check_length(complete)?;
            Ok(Plan::Write {
                start: first,
                end: Some(stop),
                complete,
            })
        }
        (Some(_), Framing::Unsatisfiable { complete }) => {
            check_length(complete)?;
            Ok(Plan::Reconcile { complete })
        }
    }
}

/// Whether durable `coverage` is exactly the whole artifact `0..size`. File
/// length never stands in for coverage (T25); an empty artifact needs no
/// coverage.
pub fn is_complete<'a>(coverage: impl IntoIterator<Item = &'a Range<u64>>, size: u64) -> bool {
    let mut ranges = coverage.into_iter();
    match (ranges.next(), ranges.next()) {
        (None, _) => size == 0,
        (Some(only), None) => only.start == 0 && only.end == size,
        (Some(_), Some(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_requests_take_the_whole_body() {
        assert_eq!(
            plan(None, Framing::Full { length: Some(10) }, None),
            Ok(Plan::Write {
                start: 0,
                end: Some(10),
                complete: Some(10)
            })
        );
        assert_eq!(
            plan(None, Framing::Full { length: None }, None),
            Ok(Plan::Write {
                start: 0,
                end: None,
                complete: None
            })
        );
        // A valid empty file is a write of nothing, not an error (T01).
        assert_eq!(
            plan(None, Framing::Full { length: Some(0) }, None),
            Ok(Plan::Write {
                start: 0,
                end: Some(0),
                complete: Some(0)
            })
        );
        assert_eq!(
            plan(
                None,
                Framing::Partial {
                    first: 0,
                    last: 9,
                    complete: Some(10)
                },
                None
            ),
            Err(RangeViolation::Unrequested)
        );
    }

    #[test]
    fn a_matching_partial_response_is_written_at_its_offset() {
        let partial = Framing::Partial {
            first: 100,
            last: 199,
            complete: Some(1000),
        };
        assert_eq!(
            plan(Some((100, Some(200))), partial, Some(1000)),
            Ok(Plan::Write {
                start: 100,
                end: Some(200),
                complete: Some(1000)
            })
        );
        // A shorter range than requested is allowed; the rest is requested
        // again later.
        assert!(plan(Some((100, None)), partial, None).is_ok());
    }

    #[test]
    fn mismatched_partial_responses_are_refused() {
        let cases = [
            (
                Some((50, None)),
                Framing::Partial {
                    first: 0,
                    last: 99,
                    complete: Some(1000),
                },
                RangeViolation::WrongStart {
                    requested: 50,
                    found: 0,
                },
            ),
            (
                Some((0, Some(50))),
                Framing::Partial {
                    first: 0,
                    last: 99,
                    complete: Some(1000),
                },
                RangeViolation::BeyondRequest,
            ),
            (
                Some((10, None)),
                Framing::Partial {
                    first: 10,
                    last: 5,
                    complete: None,
                },
                RangeViolation::Invalid,
            ),
            (
                Some((10, None)),
                Framing::Partial {
                    first: 10,
                    last: 1000,
                    complete: Some(1000),
                },
                RangeViolation::Invalid,
            ),
            (
                Some((10, None)),
                Framing::Partial {
                    first: 10,
                    last: u64::MAX,
                    complete: None,
                },
                RangeViolation::Invalid,
            ),
            (
                Some((10, None)),
                Framing::Partial {
                    first: 10,
                    last: 19,
                    complete: Some(999),
                },
                RangeViolation::LengthChanged {
                    expected: 1000,
                    found: 999,
                },
            ),
        ];
        for (requested, framing, expected) in cases {
            assert_eq!(
                plan(requested, framing, Some(1000)),
                Err(expected),
                "{framing:?}"
            );
        }
    }

    #[test]
    fn an_ignored_range_is_never_appended() {
        assert_eq!(
            plan(
                Some((500, None)),
                Framing::Full { length: Some(1000) },
                Some(1000)
            ),
            Ok(Plan::RestartFromZero { length: Some(1000) })
        );
        // Asking from zero and getting everything is the same bytes.
        assert!(matches!(
            plan(Some((0, None)), Framing::Full { length: Some(1000) }, None),
            Ok(Plan::Write { start: 0, .. })
        ));
        assert!(matches!(
            plan(
                Some((0, Some(10))),
                Framing::Full { length: Some(1000) },
                None
            ),
            Ok(Plan::RestartFromZero { .. })
        ));
    }

    #[test]
    #[expect(clippy::single_range_in_vec_init, reason = "lists of one byte range")]
    fn unsatisfiable_is_not_completion() {
        assert_eq!(
            plan(
                Some((1000, None)),
                Framing::Unsatisfiable {
                    complete: Some(1000)
                },
                Some(1000)
            ),
            Ok(Plan::Reconcile {
                complete: Some(1000)
            })
        );
        assert!(!is_complete(&[0..400, 500..1000], 1000));
        assert!(!is_complete(&[0..999], 1000));
        assert!(is_complete(&[0..1000], 1000));
        assert!(is_complete(&[], 0));
        assert!(!is_complete(&[], 1));
    }
}
