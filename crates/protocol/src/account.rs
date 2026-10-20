//! Provider accounts and their authorization, as clients see them
//! (architecture brief sections 6.1, 7.3, 8.3 to 8.5, ADR-24).
//!
//! An account's credentials never cross this boundary in either direction:
//! a client starts an authorization with the provider, the person approves
//! it on the provider's own site, and the engine receives and keeps the
//! credentials. Clients see only redacted metadata: the service, an alias,
//! whether the account is enabled, and what the provider last said about
//! it. Every account query and command needs [`Permission::Accounts`],
//! which no other permission implies.
//!
//! [`Permission::Accounts`]: crate::Permission::Accounts

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wayfetch_domain::id::AccountId;
use wayfetch_domain::name::{DiagnosticText, ProviderId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Revision;

use crate::error::{ErrorCode, ProtocolError};

/// The most accounts an engine keeps.
pub const MAX_ACCOUNTS: usize = 64;
/// The longest alias, in bytes.
pub const MAX_ALIAS_BYTES: usize = 64;

/// A name the user gives an account, to tell several apart: 1 to
/// [`MAX_ALIAS_BYTES`] bytes of text without control characters, trimmed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, type = "string"))]
pub struct AccountAlias(String);

impl AccountAlias {
    /// Validates an alias.
    pub fn new(text: &str) -> Result<Self, ProtocolError> {
        let text = text.trim();
        let invalid = |m: &str| ProtocolError::new(ErrorCode::InvalidRequest, m);
        if text.is_empty() {
            return Err(invalid("an alias must not be empty"));
        }
        if text.len() > MAX_ALIAS_BYTES {
            return Err(ProtocolError::new(
                ErrorCode::LimitExceeded,
                "an alias is at most 64 bytes",
            ));
        }
        if text.chars().any(char::is_control) {
            return Err(invalid("an alias must not contain control characters"));
        }
        Ok(Self(text.to_owned()))
    }

    /// The alias.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AccountAlias {
    type Error = ProtocolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<AccountAlias> for String {
    fn from(value: AccountAlias) -> Self {
        value.0
    }
}

/// One authorization with a provider, while the person approves it and for
/// a while after it ends. Random, so it cannot be guessed from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AuthorizationId(Uuid);

impl AuthorizationId {
    /// A fresh random identity.
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }

    /// The identity.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl std::str::FromStr for AuthorizationId {
    type Err = ProtocolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s)
            .map(Self)
            .map_err(|_| ProtocolError::new(ErrorCode::InvalidRequest, "not an authorization"))
    }
}

/// How a client signs in to a service the engine has an integration for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum AuthorizationMethod {
    /// The person enters a short code on the provider's site, from any
    /// device, and approves Wayfetch there (OAuth 2.0 device
    /// authorization).
    DeviceCode,
}

/// A service the engine can add accounts for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ServiceSupport {
    /// The service, as named in the UI's catalogue.
    pub service: ProviderId,
    /// How an account is signed in.
    pub method: AuthorizationMethod,
    /// Whether downloads can go through its accounts. Signing in is a
    /// separate capability: an account can be added before downloading
    /// through it is supported.
    pub downloads: bool,
}

/// Whether stored credentials can be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum VaultState {
    /// Credentials can be stored and read.
    Ready,
    /// No credential key is configured, so no account can be added.
    NotConfigured,
    /// The configured key is missing, unreadable, or not the key the
    /// accounts were stored with. Accounts are kept, and can still be
    /// listed, disabled, or removed, but cannot be checked or signed in
    /// until the right key is back; downloads without an account are not
    /// affected.
    Locked,
}

/// What the provider last said about an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum AccountStatus {
    /// Not checked since its credentials were stored.
    NotChecked,
    /// The provider accepted its credentials when last checked.
    Authenticated,
    /// The provider refused its credentials, or they can no longer be read.
    /// Sign in again to replace them.
    InvalidCredentials,
    /// The provider could not be reached or answered with a temporary
    /// failure when last checked. The credentials may still be good.
    Unavailable,
}

/// The kind of plan an account has, as the provider reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum AccountPlan {
    /// A paid plan.
    Premium,
    /// A free plan.
    Free,
}

/// An account, redacted: never its credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AccountSummary {
    /// The account.
    pub id: AccountId,
    /// Its service.
    pub service: ProviderId,
    /// Revision to present with the next command for this account.
    pub revision: Revision,
    /// The name the user gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub alias: Option<AccountAlias>,
    /// Whether it may be used. A disabled account is kept, but nothing
    /// contacts its provider.
    pub enabled: bool,
    /// What the provider last said.
    pub status: AccountStatus,
    /// More about the status, such as why the provider refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub problem: Option<DiagnosticText>,
    /// When the provider was last asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub checked_at: Option<UtcTimestamp>,
    /// The account's name at the provider, if the provider said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub username: Option<String>,
    /// The plan, if the provider said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub plan: Option<AccountPlan>,
    /// When the paid plan ends, if the provider said. Absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub expires_at: Option<UtcTimestamp>,
    /// When it was added.
    pub added_at: UtcTimestamp,
}

/// The accounts the user added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Accounts {
    /// Whether stored credentials can be used.
    pub vault: VaultState,
    /// Why not, if they cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub vault_problem: Option<DiagnosticText>,
    /// The services accounts can be added for.
    pub services: Vec<ServiceSupport>,
    /// The accounts, in the order they were added; at most
    /// [`MAX_ACCOUNTS`].
    pub accounts: Vec<AccountSummary>,
}

/// Where an authorization is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum AuthorizationState {
    /// Waiting for the person to approve it on the provider's site.
    Pending,
    /// Approved, and the account's credentials are stored.
    Completed,
    /// The provider refused it, or it failed; `problem` says why.
    Failed,
    /// The code expired before it was approved.
    Expired,
    /// It was cancelled, or overtaken: the account was removed, disabled,
    /// or signed in again elsewhere first.
    Cancelled,
}

/// An authorization with a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Authorization {
    /// The authorization.
    pub id: AuthorizationId,
    /// The service.
    pub service: ProviderId,
    /// Where it is.
    pub state: AuthorizationState,
    /// The code to enter on the provider's site, while pending. Short-lived
    /// and useless without the person's approval there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub user_code: Option<String>,
    /// The provider's page to enter it on, while pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub verification_url: Option<String>,
    /// When the code expires.
    pub expires_at: UtcTimestamp,
    /// The account signed in again, or added once completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub account: Option<AccountId>,
    /// Why it failed or was cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub problem: Option<DiagnosticText>,
}

/// An account, at the revision the client last saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AccountRef {
    /// The account.
    pub account: AccountId,
    /// Its revision as the client saw it.
    pub revision: Revision,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn aliases_are_trimmed_and_bounded() {
        assert_eq!(AccountAlias::new("  Home  ").unwrap().as_str(), "Home");
        assert_eq!(
            AccountAlias::new(" ").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            AccountAlias::new(&"a".repeat(MAX_ALIAS_BYTES + 1))
                .unwrap_err()
                .code,
            ErrorCode::LimitExceeded
        );
        assert!(AccountAlias::new("a\nb").is_err());
        assert!(serde_json::from_value::<AccountAlias>(json!("")).is_err());
    }

    #[test]
    fn unknown_values_are_left_out() {
        let summary = AccountSummary {
            id: AccountId::from_u64(1).unwrap(),
            service: ProviderId::new("real-debrid").unwrap(),
            revision: Revision::INITIAL,
            alias: None,
            enabled: true,
            status: AccountStatus::NotChecked,
            problem: None,
            checked_at: None,
            username: None,
            plan: None,
            expires_at: None,
            added_at: UtcTimestamp::from_unix_millis(5),
        };
        assert_eq!(
            serde_json::to_value(&summary).unwrap(),
            json!({
                "id": "acct-1",
                "service": "real-debrid",
                "revision": 1,
                "enabled": true,
                "status": "not_checked",
                "added_at": 5,
            })
        );
    }
}
