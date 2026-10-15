//! Preferences users change at run time, resolved against the operator's
//! policy (architecture brief section 11.3).
//!
//! A value comes from, in order: the built-in default, the operator's
//! configured default, and the user's saved preference, unless the
//! operator fixed it. A saved preference the policy no longer permits is
//! kept and reported as a conflict, and the nearest permitted value runs
//! instead, so the engine never runs outside the operator's restriction.

use wayfetch_protocol::command::PreferenceChange;
use wayfetch_protocol::query::{BoundedSetting, SettingSource};
use wayfetch_protocol::{ErrorCode, ProtocolError};

use crate::config::{IntegerPolicy, Preference};

/// What applies for `preference` under `policy`, given the user's `saved`
/// preference.
pub(crate) fn resolve(
    preference: Preference,
    policy: &IntegerPolicy,
    saved: Option<u64>,
) -> BoundedSetting {
    let default = policy.default_value(preference);
    let configured = if policy.default.is_some() {
        SettingSource::Server
    } else {
        SettingSource::BuiltIn
    };
    let (value, source) = match saved {
        Some(saved) if !policy.locked => {
            (saved.clamp(policy.min, policy.max), SettingSource::Saved)
        }
        _ => (default, configured),
    };
    BoundedSetting {
        value,
        source,
        default,
        saved,
        min: policy.min,
        max: policy.max,
        locked: policy.locked,
        conflict: saved.is_some_and(|saved| saved != value),
    }
}

/// Refuses a change the policy does not permit: any value for a fixed
/// preference, or one outside the permitted range. Returning to the
/// default is always permitted.
pub(crate) fn check(
    preference: Preference,
    policy: &IntegerPolicy,
    change: PreferenceChange,
) -> Result<(), ProtocolError> {
    let PreferenceChange::Set(value) = change else {
        return Ok(());
    };
    if policy.locked {
        return Err(ProtocolError::new(
            ErrorCode::NotApplicable,
            &format!("{} is managed by server configuration", preference.name),
        ));
    }
    if !(policy.min..=policy.max).contains(&value) {
        return Err(ProtocolError::new(
            ErrorCode::InvalidRequest,
            &format!(
                "{} must be between {} and {}",
                preference.name, policy.min, policy.max
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_ACTIVE: Preference = Preference::MAX_ACTIVE_DOWNLOADS;

    fn policy(default: Option<u64>, min: u64, max: u64, locked: bool) -> IntegerPolicy {
        IntegerPolicy {
            default,
            min,
            max,
            locked,
        }
    }

    #[test]
    fn the_saved_preference_beats_the_configured_default_which_beats_the_built_in_one() {
        let unrestricted = IntegerPolicy::unrestricted(MAX_ACTIVE);
        let built_in = resolve(MAX_ACTIVE, &unrestricted, None);
        assert_eq!(
            (built_in.value, built_in.source, built_in.default),
            (4, SettingSource::BuiltIn, 4)
        );
        let configured = policy(Some(6), 1, 64, false);
        let server = resolve(MAX_ACTIVE, &configured, None);
        assert_eq!((server.value, server.source), (6, SettingSource::Server));
        let saved = resolve(MAX_ACTIVE, &configured, Some(10));
        assert_eq!(
            (saved.value, saved.source, saved.saved, saved.conflict),
            (10, SettingSource::Saved, Some(10), false)
        );
        // A saved value equal to the default is still the user's choice.
        let same = resolve(MAX_ACTIVE, &configured, Some(6));
        assert_eq!((same.value, same.source), (6, SettingSource::Saved));
    }

    #[test]
    fn a_lock_wins_and_a_saved_value_outside_the_policy_is_kept_but_not_used() {
        let locked = resolve(MAX_ACTIVE, &policy(Some(2), 1, 8, true), Some(6));
        assert_eq!(
            (
                locked.value,
                locked.source,
                locked.saved,
                locked.locked,
                locked.conflict
            ),
            (2, SettingSource::Server, Some(6), true, true)
        );
        // Saving the locked value is no conflict.
        assert!(!resolve(MAX_ACTIVE, &policy(Some(2), 1, 8, true), Some(2)).conflict);
        let narrowed = resolve(MAX_ACTIVE, &policy(None, 1, 8, false), Some(20));
        assert_eq!(
            (narrowed.value, narrowed.saved, narrowed.conflict),
            (8, Some(20), true)
        );
        let raised = resolve(MAX_ACTIVE, &policy(Some(4), 3, 8, false), Some(1));
        assert_eq!((raised.value, raised.conflict), (3, true));
    }

    #[test]
    fn changes_outside_the_policy_are_refused() {
        let bounded = policy(None, 2, 8, false);
        assert_eq!(
            check(MAX_ACTIVE, &bounded, PreferenceChange::Set(8)),
            Ok(())
        );
        for value in [0, 1, 9, u64::MAX] {
            assert_eq!(
                check(MAX_ACTIVE, &bounded, PreferenceChange::Set(value))
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidRequest
            );
        }
        let locked = policy(Some(4), 1, 64, true);
        assert_eq!(
            check(MAX_ACTIVE, &locked, PreferenceChange::Set(4))
                .unwrap_err()
                .code,
            ErrorCode::NotApplicable
        );
        assert_eq!(
            check(MAX_ACTIVE, &locked, PreferenceChange::UseDefault),
            Ok(())
        );
    }
}
