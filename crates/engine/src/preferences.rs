//! Preferences users change at run time, resolved against the operator's
//! policy (architecture brief section 11.3).
//!
//! A value comes from, in order: the built-in default, the operator's
//! configured default, and the user's saved preference, unless the
//! operator fixed it. A saved preference the policy no longer permits is
//! kept and reported as a conflict, and the nearest permitted value runs
//! instead, so the engine never runs outside the operator's restriction.

use wayfetch_protocol::command::{PreferenceChange, SpeedLimitChange};
use wayfetch_protocol::query::{BoundedSetting, SettingSource, SpeedLimit, SpeedLimitSetting};
use wayfetch_protocol::{ErrorCode, ProtocolError};

use crate::config::{IntegerPolicy, Preference, SpeedLimitPolicy};

/// Where a value without a saved preference comes from.
fn configured<T>(default: Option<T>) -> SettingSource {
    if default.is_some() {
        SettingSource::Server
    } else {
        SettingSource::BuiltIn
    }
}

/// Refuses any change but returning to the default for a fixed preference.
fn unlocked(name: &str, locked: bool) -> Result<(), ProtocolError> {
    if locked {
        return Err(ProtocolError::new(
            ErrorCode::NotApplicable,
            &format!("{name} is managed by server configuration"),
        ));
    }
    Ok(())
}

/// What applies for `preference` under `policy`, given the user's `saved`
/// preference.
pub(crate) fn resolve(
    preference: Preference,
    policy: &IntegerPolicy,
    saved: Option<u64>,
) -> BoundedSetting {
    let default = policy.default_value(preference);
    let (value, source) = match saved {
        Some(saved) if !policy.locked => {
            (saved.clamp(policy.min, policy.max), SettingSource::Saved)
        }
        _ => (default, configured(policy.default)),
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
    unlocked(preference.name, policy.locked)?;
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

/// What applies for the total download speed limit under `policy`, given
/// the user's `saved` preference. A saved limit outside the policy runs as
/// the nearest permitted one; a saved Unlimited under a maximum runs at
/// the maximum.
pub(crate) fn resolve_speed_limit(
    policy: &SpeedLimitPolicy,
    saved: Option<SpeedLimit>,
) -> SpeedLimitSetting {
    let default = policy.default_value();
    let (value, source) = match saved {
        Some(saved) if !policy.locked => (policy.nearest(saved), SettingSource::Saved),
        _ => (default, configured(policy.default)),
    };
    SpeedLimitSetting {
        value,
        source,
        default,
        saved,
        min: policy.min,
        max: policy.ceiling(),
        unlimited: policy.max.is_none(),
        locked: policy.locked,
        conflict: saved.is_some_and(|saved| saved != value),
    }
}

/// Refuses a speed limit change the policy does not permit: any limit
/// when it is fixed, a finite one outside the range, or Unlimited under a
/// maximum. Returning to the default is always permitted.
pub(crate) fn check_speed_limit(
    policy: &SpeedLimitPolicy,
    change: SpeedLimitChange,
) -> Result<(), ProtocolError> {
    let SpeedLimitChange::Set(limit) = change else {
        return Ok(());
    };
    unlocked(SpeedLimitPolicy::NAME, policy.locked)?;
    if policy.permits(limit) {
        return Ok(());
    }
    let message = match limit {
        SpeedLimit::Unlimited => format!(
            "{} cannot be unlimited: the server's configuration allows at most {} bytes per second",
            SpeedLimitPolicy::NAME,
            policy.ceiling()
        ),
        SpeedLimit::BytesPerSecond(_) => format!(
            "{} must be between {} and {} bytes per second",
            SpeedLimitPolicy::NAME,
            policy.min,
            policy.ceiling()
        ),
    };
    Err(ProtocolError::new(ErrorCode::InvalidRequest, &message))
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

    const MIB: u64 = 1 << 20;

    fn rate(bytes: u64) -> SpeedLimit {
        SpeedLimit::BytesPerSecond(bytes)
    }

    fn speed(
        default: Option<SpeedLimit>,
        min: u64,
        max: Option<u64>,
        locked: bool,
    ) -> SpeedLimitPolicy {
        SpeedLimitPolicy {
            default,
            min,
            max,
            locked,
        }
    }

    #[test]
    fn unlimited_is_the_built_in_speed_limit_and_differs_from_no_preference() {
        let built_in = resolve_speed_limit(&SpeedLimitPolicy::UNRESTRICTED, None);
        assert_eq!(
            (
                built_in.value,
                built_in.source,
                built_in.saved,
                built_in.unlimited
            ),
            (SpeedLimit::Unlimited, SettingSource::BuiltIn, None, true)
        );
        assert_eq!(built_in.max, SpeedLimitPolicy::MAX);
        // A saved Unlimited is the user's choice, kept when the configured
        // default changes.
        let configured = speed(Some(rate(2 * MIB)), 1024, None, false);
        let server = resolve_speed_limit(&configured, None);
        assert_eq!(
            (server.value, server.source),
            (rate(2 * MIB), SettingSource::Server)
        );
        let saved = resolve_speed_limit(&configured, Some(SpeedLimit::Unlimited));
        assert_eq!(
            (saved.value, saved.source, saved.saved, saved.conflict),
            (
                SpeedLimit::Unlimited,
                SettingSource::Saved,
                Some(SpeedLimit::Unlimited),
                false
            )
        );
        let same = resolve_speed_limit(&configured, Some(rate(2 * MIB)));
        assert_eq!((same.source, same.conflict), (SettingSource::Saved, false));
    }

    #[test]
    fn a_ceiling_holds_against_a_saved_unlimited_and_a_lock_wins() {
        let capped = speed(Some(rate(MIB)), 1024, Some(4 * MIB), false);
        let unlimited = resolve_speed_limit(&capped, Some(SpeedLimit::Unlimited));
        assert_eq!(
            (
                unlimited.value,
                unlimited.conflict,
                unlimited.unlimited,
                unlimited.max
            ),
            (rate(4 * MIB), true, false, 4 * MIB)
        );
        let high = resolve_speed_limit(&capped, Some(rate(8 * MIB)));
        assert_eq!((high.value, high.conflict), (rate(4 * MIB), true));
        let low = resolve_speed_limit(&speed(None, 64 * 1024, None, false), Some(rate(1024)));
        assert_eq!((low.value, low.conflict), (rate(64 * 1024), true));

        let locked = speed(Some(rate(MIB)), 1024, None, true);
        let fixed = resolve_speed_limit(&locked, Some(SpeedLimit::Unlimited));
        assert_eq!(
            (fixed.value, fixed.source, fixed.locked, fixed.conflict),
            (rate(MIB), SettingSource::Server, true, true)
        );
        assert!(!resolve_speed_limit(&locked, Some(rate(MIB))).conflict);
    }

    #[test]
    fn speed_limit_changes_outside_the_policy_are_refused() {
        let capped = speed(Some(rate(MIB)), 2048, Some(4 * MIB), false);
        let refused =
            |policy: &SpeedLimitPolicy, change| check_speed_limit(policy, change).unwrap_err().code;
        assert_eq!(
            check_speed_limit(&capped, SpeedLimitChange::Set(rate(4 * MIB))),
            Ok(())
        );
        assert_eq!(
            check_speed_limit(&capped, SpeedLimitChange::Set(rate(2048))),
            Ok(())
        );
        for limit in [
            rate(0),
            rate(2047),
            rate(4 * MIB + 1),
            rate(u64::MAX),
            SpeedLimit::Unlimited,
        ] {
            assert_eq!(
                refused(&capped, SpeedLimitChange::Set(limit)),
                ErrorCode::InvalidRequest,
                "{limit:?}"
            );
        }
        let open = SpeedLimitPolicy::UNRESTRICTED;
        assert_eq!(
            check_speed_limit(&open, SpeedLimitChange::Set(SpeedLimit::Unlimited)),
            Ok(())
        );
        assert_eq!(
            refused(
                &open,
                SpeedLimitChange::Set(rate(SpeedLimitPolicy::MAX + 1))
            ),
            ErrorCode::InvalidRequest
        );
        let locked = speed(Some(rate(MIB)), 1024, None, true);
        assert_eq!(
            refused(&locked, SpeedLimitChange::Set(rate(MIB))),
            ErrorCode::NotApplicable
        );
        assert_eq!(
            check_speed_limit(&locked, SpeedLimitChange::UseDefault),
            Ok(())
        );
    }
}
