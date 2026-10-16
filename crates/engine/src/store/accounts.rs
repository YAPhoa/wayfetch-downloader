//! Provider accounts and their sealed credentials (architecture brief
//! sections 6.1, 7.3, 8.4, ADR-24).
//!
//! Only redacted metadata is stored in the clear. Credentials arrive here
//! already sealed by the vault, and leave sealed; this module never sees a
//! key or a plaintext secret.
//!
//! Two counters guard an account. `revision` is what a client presents to
//! change it. `credential_generation` advances whenever its credentials
//! change, and work that read older credentials (a status check, a
//! refresh, a sign-in that started before another finished) can only store
//! its result while the generation it read is current, so a late answer
//! never restores obsolete credentials or status.

use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::id::AccountId;
use wayfetch_domain::name::{DiagnosticText, ProviderId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::{Generation, Revision};
use wayfetch_protocol::account::{
    AccountAlias, AccountPlan, AccountStatus, AccountSummary, MAX_ACCOUNTS,
};
use wayfetch_protocol::{CommandEnvelope, CommandResult, ErrorCode, InstanceId, ProtocolError};

use super::commands::{record, replayed};
use super::{Store, StoreError, key, sql};
use crate::accounts::provider::Profile;
use crate::accounts::vault::{FORMAT, KeyCheck, REFERENCE_BYTES, Reference};

/// Longest provider user ID or username stored; longer ones are dropped.
const MAX_PROVIDER_TEXT: usize = 128;

/// An account as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountRow {
    pub(crate) id: AccountId,
    pub(crate) service: ProviderId,
    pub(crate) alias: Option<AccountAlias>,
    pub(crate) revision: Revision,
    pub(crate) generation: Generation,
    pub(crate) enabled: bool,
    pub(crate) status: AccountStatus,
    pub(crate) problem: Option<DiagnosticText>,
    pub(crate) checked_at: Option<UtcTimestamp>,
    pub(crate) provider_user: Option<String>,
    pub(crate) username: Option<String>,
    pub(crate) plan: Option<AccountPlan>,
    pub(crate) expires_at: Option<UtcTimestamp>,
    pub(crate) added_at: UtcTimestamp,
}

impl AccountRow {
    /// What a client sees: never credentials, nor the provider's internal
    /// user ID.
    pub(crate) fn summary(&self) -> AccountSummary {
        AccountSummary {
            id: self.id,
            service: self.service.clone(),
            revision: self.revision,
            alias: self.alias.clone(),
            enabled: self.enabled,
            status: self.status,
            problem: self.problem.clone(),
            checked_at: self.checked_at,
            username: self.username.clone(),
            plan: self.plan,
            expires_at: self.expires_at,
            added_at: self.added_at,
        }
    }
}

/// A sealed credential record and its opaque reference.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct StoredCredential {
    pub(crate) reference: Reference,
    pub(crate) sealed: Vec<u8>,
}

impl std::fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StoredCredential([sealed])")
    }
}

/// What checking an account with its provider found.
#[derive(Debug)]
pub(crate) enum CheckResult {
    /// The provider accepted the credentials; `refreshed` replaces them.
    Authenticated {
        profile: Profile,
        refreshed: Option<StoredCredential>,
    },
    /// The provider refused them, failed, or they could not be read.
    /// `refreshed` still replaces them: a refresh before the failure may
    /// have made the old ones unusable.
    Failed {
        status: AccountStatus,
        problem: String,
        refreshed: Option<StoredCredential>,
    },
}

const COLUMNS: &str = "id, service, alias, revision, credential_generation, enabled, status, \
                       problem, checked_at, provider_user, username, plan, expires_at, added_at";

type Raw = (
    (i64, String, Option<String>, i64, i64, bool, String),
    (
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        i64,
    ),
);

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Raw> {
    Ok((
        (
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
        ),
        (
            row.get(7)?,
            row.get(8)?,
            row.get(9)?,
            row.get(10)?,
            row.get(11)?,
            row.get(12)?,
            row.get(13)?,
        ),
    ))
}

fn decode(raw: Raw) -> Result<AccountRow, StoreError> {
    let (
        (id, service, alias, revision, generation, enabled, status),
        (problem, checked_at, provider_user, username, plan, expires_at, added_at),
    ) = raw;
    let corrupt = |_| StoreError::Corrupt("account");
    Ok(AccountRow {
        id: key(id)?,
        service: ProviderId::new(service).map_err(|_| StoreError::Corrupt("account service"))?,
        alias: alias
            .map(|a| AccountAlias::new(&a))
            .transpose()
            .map_err(corrupt)?,
        revision: key(revision)?,
        generation: key(generation)?,
        enabled,
        status: from_text(&status).ok_or(StoreError::Corrupt("account status"))?,
        problem: problem.map(|p| DiagnosticText::new(&p)),
        checked_at: checked_at.map(UtcTimestamp::from_unix_millis),
        provider_user,
        username,
        plan: plan
            .map(|p| from_text(&p).ok_or(StoreError::Corrupt("account plan")))
            .transpose()?,
        expires_at: expires_at.map(UtcTimestamp::from_unix_millis),
        added_at: UtcTimestamp::from_unix_millis(added_at),
    })
}

/// A unit enum's stored text: its serde name.
fn text<T: serde::Serialize>(value: T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => unreachable!("a unit variant serializes as a string"),
    }
}

fn from_text<T: serde::de::DeserializeOwned>(stored: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(stored.to_owned())).ok()
}

/// Provider text kept only if it is short and printable.
fn bounded(value: Option<&str>) -> Option<&str> {
    value.filter(|v| {
        !v.is_empty() && v.len() <= MAX_PROVIDER_TEXT && !v.chars().any(char::is_control)
    })
}

/// The provider's ID for the account a sign-in reached, which must be
/// known before any credentials are stored or replaced.
fn confirmed(profile: &Profile) -> Result<&str, ProtocolError> {
    bounded(profile.user_id.as_deref()).ok_or_else(|| {
        ProtocolError::new(
            ErrorCode::NotApplicable,
            "the service did not confirm which account signed in; try again",
        )
    })
}

fn load(tx: &Transaction<'_>, id: AccountId) -> Result<Option<AccountRow>, StoreError> {
    tx.query_row(
        &format!("SELECT {COLUMNS} FROM accounts WHERE id = ?1"),
        [sql(id)?],
        read_row,
    )
    .optional()?
    .map(decode)
    .transpose()
}

fn load_existing(tx: &Transaction<'_>, id: AccountId) -> Result<AccountRow, StoreError> {
    load(tx, id)?.ok_or(StoreError::NotFound("account"))
}

fn revision_conflict(current: Revision) -> ProtocolError {
    ProtocolError {
        current_revision: Some(current),
        ..ProtocolError::new(
            ErrorCode::RevisionConflict,
            "the account changed since it was read",
        )
    }
}

fn next<C: Copy>(
    value: C,
    step: impl FnOnce(C) -> Result<C, wayfetch_domain::version::CounterExhausted>,
) -> Result<C, StoreError> {
    step(value).map_err(|_| StoreError::Corrupt("account counter"))
}

fn write_credential(
    tx: &Transaction<'_>,
    account: AccountId,
    credential: &StoredCredential,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO account_credentials (account_id, reference, format, sealed) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (account_id) DO UPDATE SET \
         reference = excluded.reference, format = excluded.format, sealed = excluded.sealed",
        params![
            sql(account)?,
            &credential.reference[..],
            FORMAT,
            &credential.sealed
        ],
    )?;
    Ok(())
}

/// Stores what the provider said about an account that it accepted.
fn write_profile(
    tx: &Transaction<'_>,
    account: AccountId,
    profile: &Profile,
    now: UtcTimestamp,
) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE accounts SET status = 'authenticated', problem = NULL, checked_at = ?2, \
         provider_user = COALESCE(provider_user, ?3), username = ?4, plan = ?5, \
         expires_at = ?6 WHERE id = ?1",
        params![
            sql(account)?,
            now.unix_millis(),
            bounded(profile.user_id.as_deref()),
            bounded(profile.username.as_deref()),
            profile.plan.map(text),
            profile.expires_at.map(UtcTimestamp::unix_millis),
        ],
    )?;
    Ok(())
}

/// Whether another account of `service` is the provider's `user`.
fn duplicate(
    tx: &Transaction<'_>,
    service: &ProviderId,
    user: Option<&str>,
    except: Option<AccountId>,
) -> Result<bool, StoreError> {
    let Some(user) = bounded(user) else {
        return Ok(false);
    };
    let found: Option<i64> = tx
        .query_row(
            "SELECT id FROM accounts WHERE service = ?1 AND provider_user = ?2",
            params![service.as_str(), user],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match (found, except) {
        (None, _) => false,
        (Some(found), Some(except)) => found != sql(except)?,
        (Some(_), None) => true,
    })
}

fn already_added() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::AlreadyExists,
        "this account was already added; sign it in again from its row instead",
    )
}

impl Store {
    /// The check of the vault's key, once one was stored.
    pub(crate) fn key_check(&mut self) -> Result<Option<KeyCheck>, StoreError> {
        let tx = self.read()?;
        let check: Option<Vec<u8>> = tx
            .query_row(
                "SELECT key_check FROM credential_vault WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        check
            .map(|c| c.try_into().map_err(|_| StoreError::Corrupt("key check")))
            .transpose()
    }

    /// Stores the check of the vault's first key. A different check already
    /// stored is refused: the vault's key is never replaced.
    pub(crate) fn store_key_check(
        &mut self,
        check: &KeyCheck,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        tx.execute(
            "INSERT INTO credential_vault (id, key_check, created_at) VALUES (1, ?1, ?2) \
             ON CONFLICT (id) DO NOTHING",
            params![&check[..], now.unix_millis()],
        )?;
        let stored: Vec<u8> = tx.query_row(
            "SELECT key_check FROM credential_vault WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        if stored != check {
            return Err(StoreError::Corrupt("key check"));
        }
        tx.commit()?;
        Ok(())
    }

    /// Every account, in the order they were added.
    pub(crate) fn accounts(&mut self) -> Result<Vec<AccountRow>, StoreError> {
        let tx = self.read()?;
        let rows = tx
            .prepare(&format!(
                "SELECT {COLUMNS} FROM accounts ORDER BY id LIMIT ?1"
            ))?
            .query_map([MAX_ACCOUNTS as i64], read_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(decode).collect()
    }

    /// One account.
    pub(crate) fn account(&mut self, id: AccountId) -> Result<AccountRow, StoreError> {
        let tx = self.read()?;
        load_existing(&tx, id)
    }

    /// One account and its sealed credentials.
    pub(crate) fn account_credential(
        &mut self,
        id: AccountId,
    ) -> Result<(AccountRow, StoredCredential), StoreError> {
        let tx = self.read()?;
        let row = load_existing(&tx, id)?;
        let (reference, sealed): (Vec<u8>, Vec<u8>) = tx
            .query_row(
                "SELECT reference, sealed FROM account_credentials WHERE account_id = ?1",
                [sql(id)?],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound("account credentials"))?;
        let reference: [u8; REFERENCE_BYTES] = reference
            .try_into()
            .map_err(|_| StoreError::Corrupt("credential reference"))?;
        Ok((row, StoredCredential { reference, sealed }))
    }

    /// The stored result of an account command already accepted under the
    /// envelope's key, after checking the envelope is for this instance.
    pub(crate) fn replayed_command(
        &mut self,
        envelope: &CommandEnvelope,
        instance: InstanceId,
    ) -> Result<Option<CommandResult>, ProtocolError> {
        let tx = self.read()?;
        replayed(&tx, envelope, instance)
    }

    /// Adds the account an authorization signed in, with its credentials
    /// and what the provider said, recording the authorization's command
    /// under its key. A retry of a command already recorded only replays
    /// its result. Refused without the provider's ID for the account, or
    /// if that user is already an account.
    pub(crate) fn add_account(
        &mut self,
        envelope: &CommandEnvelope,
        instance: InstanceId,
        credential: &StoredCredential,
        profile: &Profile,
        now: UtcTimestamp,
    ) -> Result<CommandResult, ProtocolError> {
        let wayfetch_protocol::Command::AuthorizeAccount { service, alias, .. } = &envelope.command
        else {
            return Err(ProtocolError::new(
                ErrorCode::Internal,
                "not an authorization",
            ));
        };
        let tx = self.write()?;
        if let Some(result) = replayed(&tx, envelope, instance)? {
            return Ok(result);
        }
        let count: i64 = tx
            .query_row("SELECT count(*) FROM accounts", [], |row| row.get(0))
            .map_err(StoreError::from)?;
        if count >= MAX_ACCOUNTS as i64 {
            return Err(ProtocolError::new(
                ErrorCode::LimitExceeded,
                "no more accounts can be added",
            ));
        }
        let user = confirmed(profile)?;
        if duplicate(&tx, service, Some(user), None)? {
            return Err(already_added());
        }
        tx.execute(
            "INSERT INTO accounts (service, alias, revision, credential_generation, enabled, \
             status, added_at) VALUES (?1, ?2, 1, 1, 1, 'not_checked', ?3)",
            params![
                service.as_str(),
                alias.as_ref().map(AccountAlias::as_str),
                now.unix_millis()
            ],
        )
        .map_err(StoreError::from)?;
        let account: AccountId = key(tx.last_insert_rowid())?;
        write_credential(&tx, account, credential)?;
        write_profile(&tx, account, profile, now)?;
        let result = CommandResult::AccountAuthorized {
            account,
            revision: Revision::INITIAL,
        };
        record(&tx, envelope, &result, now)?;
        tx.commit().map_err(StoreError::from)?;
        Ok(result)
    }

    /// Replaces an account's credentials with those an authorization
    /// signed in, if they are still the generation it started from and the
    /// account is enabled. Advances its revision and generation, so work
    /// with the old credentials can no longer store anything.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replace_credentials(
        &mut self,
        envelope: &CommandEnvelope,
        instance: InstanceId,
        account: AccountId,
        generation: Generation,
        credential: &StoredCredential,
        profile: &Profile,
        now: UtcTimestamp,
    ) -> Result<CommandResult, ProtocolError> {
        let tx = self.write()?;
        if let Some(result) = replayed(&tx, envelope, instance)? {
            return Ok(result);
        }
        let row = load(&tx, account)?.ok_or_else(|| {
            ProtocolError::new(ErrorCode::NotFound, "the account was removed meanwhile")
        })?;
        // Overtaken by another change to the account.
        if row.generation != generation {
            return Err(ProtocolError::new(
                ErrorCode::RevisionConflict,
                "the account was signed in again meanwhile",
            ));
        }
        if !row.enabled {
            return Err(ProtocolError::new(
                ErrorCode::RevisionConflict,
                "the account was disabled meanwhile",
            ));
        }
        // Only the same user at the provider may replace the credentials.
        let user = confirmed(profile)?;
        if row.provider_user.as_deref() != Some(user) {
            return Err(ProtocolError::new(
                ErrorCode::NotApplicable,
                "that is a different account at the provider; add it as a new account instead",
            ));
        }
        if duplicate(&tx, &row.service, Some(user), Some(account))? {
            return Err(already_added());
        }
        let revision = next(row.revision, Revision::next)?;
        tx.execute(
            "UPDATE accounts SET revision = ?2, credential_generation = ?3, \
             status = 'not_checked', problem = NULL, checked_at = NULL WHERE id = ?1",
            params![
                sql(account)?,
                sql(revision)?,
                sql(next(generation, Generation::next)?)?
            ],
        )
        .map_err(StoreError::from)?;
        write_credential(&tx, account, credential)?;
        write_profile(&tx, account, profile, now)?;
        let result = CommandResult::AccountAuthorized { account, revision };
        record(&tx, envelope, &result, now)?;
        tx.commit().map_err(StoreError::from)?;
        Ok(result)
    }

    /// Stores credentials a provider call refreshed, if the account's
    /// credentials are still the generation the call read, advancing it.
    /// Returns whether they were stored.
    pub(crate) fn store_refreshed(
        &mut self,
        account: AccountId,
        generation: Generation,
        credential: &StoredCredential,
    ) -> Result<bool, StoreError> {
        let tx = self.write()?;
        let Some(row) = load(&tx, account)? else {
            return Ok(false);
        };
        if row.generation != generation {
            return Ok(false);
        }
        tx.execute(
            "UPDATE accounts SET credential_generation = ?2 WHERE id = ?1",
            params![sql(account)?, sql(next(generation, Generation::next)?)?],
        )?;
        write_credential(&tx, account, credential)?;
        tx.commit()?;
        Ok(true)
    }

    /// Stores what checking an account found, if its credentials are still
    /// the generation the check read; refreshed credentials advance it.
    /// Returns the account as it is now, `None` once removed.
    pub(crate) fn record_check(
        &mut self,
        account: AccountId,
        generation: Generation,
        result: &CheckResult,
        now: UtcTimestamp,
    ) -> Result<Option<AccountRow>, StoreError> {
        let tx = self.write()?;
        let Some(row) = load(&tx, account)? else {
            return Ok(None);
        };
        if row.generation != generation {
            return Ok(Some(row));
        }
        let (CheckResult::Authenticated { refreshed, .. } | CheckResult::Failed { refreshed, .. }) =
            result;
        if let Some(credential) = refreshed {
            tx.execute(
                "UPDATE accounts SET credential_generation = ?2 WHERE id = ?1",
                params![sql(account)?, sql(next(generation, Generation::next)?)?],
            )?;
            write_credential(&tx, account, credential)?;
        }
        match result {
            CheckResult::Authenticated { profile, .. } => {
                write_profile(&tx, account, profile, now)?;
            }
            CheckResult::Failed {
                status, problem, ..
            } => {
                tx.execute(
                    "UPDATE accounts SET status = ?2, problem = ?3, checked_at = ?4 WHERE id = ?1",
                    params![
                        sql(account)?,
                        text(*status),
                        DiagnosticText::new(problem).as_str(),
                        now.unix_millis()
                    ],
                )?;
            }
        }
        let row = load_existing(&tx, account)?;
        tx.commit()?;
        Ok(Some(row))
    }
}

/// Enables or disables an account at the revision the client saw.
pub(super) fn set_enabled(
    tx: &Transaction<'_>,
    account: AccountId,
    expected: Revision,
    enabled: bool,
) -> Result<CommandResult, ProtocolError> {
    let row = load_existing(tx, account)?;
    if row.revision != expected {
        return Err(revision_conflict(row.revision));
    }
    if row.enabled == enabled {
        return Ok(CommandResult::AccountUpdated {
            account,
            revision: row.revision,
            changed: false,
        });
    }
    let revision = next(row.revision, Revision::next)?;
    tx.execute(
        "UPDATE accounts SET enabled = ?2, revision = ?3 WHERE id = ?1",
        params![sql(account)?, enabled, sql(revision)?],
    )
    .map_err(StoreError::from)?;
    Ok(CommandResult::AccountUpdated {
        account,
        revision,
        changed: true,
    })
}

/// Removes an account and its credentials at the revision the client saw.
/// The rows are deleted; copies in backups or on the disk's free space are
/// beyond the store's reach, and hold only sealed records.
pub(super) fn remove(
    tx: &Transaction<'_>,
    account: AccountId,
    expected: Revision,
) -> Result<CommandResult, ProtocolError> {
    let row = load_existing(tx, account)?;
    if row.revision != expected {
        return Err(revision_conflict(row.revision));
    }
    tx.execute(
        "DELETE FROM account_credentials WHERE account_id = ?1",
        [sql(account)?],
    )
    .map_err(StoreError::from)?;
    tx.execute("DELETE FROM accounts WHERE id = ?1", [sql(account)?])
        .map_err(StoreError::from)?;
    // Links generated through it are erased with it: the vault's key may
    // be forgotten below.
    super::operations::retire_account(tx, account)?;
    // With no account left, the vault holds nothing a key could open, so
    // it forgets the key's check: the next account may use a new key. A
    // key is never replaced while it protects anything.
    tx.execute(
        "DELETE FROM credential_vault WHERE NOT EXISTS (SELECT 1 FROM accounts)",
        [],
    )
    .map_err(StoreError::from)?;
    Ok(CommandResult::AccountUpdated {
        account,
        revision: row.revision,
        changed: true,
    })
}
