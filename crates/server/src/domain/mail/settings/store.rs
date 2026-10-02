//! Persist settings and their admission policy in one writer transaction.

use sqlx::{Row as _, Sqlite, SqlitePool, Transaction, sqlite::SqliteRow};
use time::OffsetDateTime;

use super::{SettingsActivation, StoredMailSettings, UpdateMailSettings};
use crate::{
    database::fingerprint::CommandFingerprintBuilder,
    domain::{
        auth::store::{append_success_audit, decode_audit_principal, require_fresh_browser_owner},
        mail::{
            config::{NewsletterSettingsCandidate, SubscriptionCandidate, SubscriptionMode},
            subscriber::{
                SubscriberCommandError, SubscriberMode, SubscriberPolicy,
                store::{SubscriberApplyError, SubscriberLoadError, auth_error, set_policy},
            },
        },
    },
};

const ACTION: &str = "mail.settings.update";
const MAX_RECEIPTS: i64 = 10_000;

pub(in crate::domain::mail) async fn load(
    readers: &SqlitePool,
) -> Result<Option<StoredMailSettings>, SubscriberLoadError> {
    // Bound copies even if a damaged database bypassed its schema constraints.
    let row = sqlx::query("SELECT version,CASE WHEN length(mode)<=7 THEN mode END AS mode,CASE WHEN length(CAST(operator_name AS BLOB))<=200 THEN operator_name END AS operator_name,CASE WHEN length(CAST(postal_address AS BLOB))<=500 THEN postal_address END AS postal_address,CASE WHEN length(CAST(purpose AS BLOB))<=2000 THEN purpose END AS purpose,CASE WHEN length(CAST(privacy_url AS BLOB))<=2048 THEN privacy_url END AS privacy_url,CASE WHEN length(CAST(contact_address AS BLOB))<=254 THEN contact_address END AS contact_address,max_campaign_recipients,max_daily_messages,max_daily_confirmation_messages,send_interval_milliseconds,(postal_address IS NULL OR length(CAST(postal_address AS BLOB))<=500) AS valid_postal_width FROM mail_settings WHERE singleton=1")
        .fetch_optional(readers).await?;
    row.as_ref().map(decode).transpose()
}

fn decode(row: &SqliteRow) -> Result<StoredMailSettings, SubscriberLoadError> {
    let version = positive(row, "version")?;
    if !row.try_get::<bool, _>("valid_postal_width")? {
        return Err(SubscriberLoadError::CorruptStoredState);
    }
    let mode = match row.try_get::<&str, _>("mode")? {
        "paused" => SubscriptionMode::Paused,
        "enabled" => SubscriptionMode::Enabled,
        _ => return Err(SubscriberLoadError::CorruptStoredState),
    };
    let settings = NewsletterSettingsCandidate {
        subscriptions: SubscriptionCandidate {
            mode,
            operator_name: row.try_get("operator_name")?,
            postal_address: row.try_get("postal_address")?,
            purpose: row.try_get("purpose")?,
            privacy_url: row.try_get("privacy_url")?,
            contact_address: row.try_get("contact_address")?,
        },
        max_campaign_recipients: positive(row, "max_campaign_recipients")?,
        max_daily_messages: positive(row, "max_daily_messages")?,
        max_daily_confirmation_messages: positive(row, "max_daily_confirmation_messages")?,
        send_interval_milliseconds: positive(row, "send_interval_milliseconds")?,
    }
    .validate()
    .map_err(|_| SubscriberLoadError::CorruptStoredState)?;
    Ok(StoredMailSettings { version, settings })
}

fn positive(row: &SqliteRow, name: &str) -> Result<u64, SubscriberLoadError> {
    u64::try_from(row.try_get::<i64, _>(name)?)
        .ok()
        .filter(|value| *value > 0)
        .ok_or(SubscriberLoadError::CorruptStoredState)
}

fn mode_name(mode: SubscriptionMode) -> &'static str {
    match mode {
        SubscriptionMode::Paused => "paused",
        SubscriptionMode::Enabled => "enabled",
    }
}

fn fingerprint(command: &UpdateMailSettings) -> [u8; 32] {
    let mut fingerprint = CommandFingerprintBuilder::new(ACTION);
    fingerprint.version(command.expected_version);
    match &command.activation {
        SettingsActivation::Offline => fingerprint.field(b"offline"),
        SettingsActivation::Live {
            expected_control_version,
            expected_binding,
            ..
        } => {
            fingerprint.field(b"live");
            fingerprint.version(*expected_control_version);
            fingerprint.field(expected_binding);
        }
    }
    let view = command.settings.view();
    let policy = view.subscriptions.view();
    for value in [
        mode_name(policy.mode),
        policy.operator_name,
        policy.purpose,
        policy.privacy_url.as_str(),
        policy.contact_address.as_str(),
    ] {
        fingerprint.field(value.as_bytes());
    }
    fingerprint.optional_field(policy.postal_address.map(str::as_bytes));
    for value in [
        view.max_campaign_recipients,
        view.max_daily_messages,
        view.max_daily_confirmation_messages,
        view.send_interval_milliseconds,
    ] {
        fingerprint.version(value);
    }
    fingerprint.finish()
}

async fn replay(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &UpdateMailSettings,
    fingerprint: [u8; 32],
) -> Result<Option<u64>, SubscriberApplyError> {
    let row = sqlx::query("SELECT audit.principal_kind,audit.actor_user_id,audit.session_id,audit.agent_credential_id,audit.action,audit.outcome,receipt.command_fingerprint,receipt.version FROM admin_audit_events AS audit LEFT JOIN mail_settings_receipts AS receipt ON receipt.audit_event_id=audit.audit_event_id WHERE audit.idempotency_key=?")
        .bind(command.audit.idempotency_key.0.as_bytes().as_slice()).fetch_optional(&mut **transaction).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let principal = decode_audit_principal(
        row.try_get("principal_kind")?,
        row.try_get("actor_user_id")?,
        row.try_get("session_id")?,
        row.try_get("agent_credential_id")?,
    )
    .map_err(|_| SubscriberApplyError::CorruptStoredState)?;
    if principal != command.audit.principal
        || row.try_get::<&str, _>("action")? != ACTION
        || row.try_get::<&str, _>("outcome")? != "succeeded"
        || row.try_get::<Option<&[u8]>, _>("command_fingerprint")? != Some(fingerprint.as_slice())
    {
        return Err(SubscriberCommandError::IdempotencyConflict.into());
    }
    let version =
        positive(&row, "version").map_err(|_| SubscriberApplyError::CorruptStoredState)?;
    Ok(Some(version))
}

pub(crate) async fn update(
    transaction: &mut Transaction<'_, Sqlite>,
    command: UpdateMailSettings,
    executed_at: OffsetDateTime,
) -> Result<u64, SubscriberApplyError> {
    let fingerprint = fingerprint(&command);
    if let Some(version) = replay(transaction, &command, fingerprint).await? {
        return Ok(version);
    }
    require_fresh_browser_owner(transaction, &command.audit.principal, executed_at)
        .await
        .map_err(auth_error)?;
    let current: Option<i64> =
        sqlx::query_scalar("SELECT version FROM mail_settings WHERE singleton=1")
            .fetch_optional(&mut **transaction)
            .await?;
    if current.unwrap_or(0)
        != i64::try_from(command.expected_version)
            .map_err(|_| SubscriberCommandError::InvalidValue)?
    {
        return Err(SubscriberCommandError::StaleVersion.into());
    }
    validate_activation(transaction, &command).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mail_settings_receipts")
        .fetch_one(&mut **transaction)
        .await?;
    if count >= MAX_RECEIPTS {
        return Err(SubscriberCommandError::Capacity.into());
    }
    let version = current
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(SubscriberCommandError::Capacity)?;
    persist(transaction, &command, version).await?;
    apply_activation(transaction, &command, executed_at).await?;
    append_success_audit(transaction, &command.audit, executed_at, ACTION)
        .await
        .map_err(auth_error)?;
    sqlx::query("INSERT INTO mail_settings_receipts(idempotency_key,audit_event_id,command_fingerprint,version) VALUES(?,?,?,?)")
        .bind(command.audit.idempotency_key.0.as_bytes().as_slice())
        .bind(command.audit.audit_event_id.as_uuid().as_bytes().as_slice())
        .bind(fingerprint.as_slice()).bind(version).execute(&mut **transaction).await?;
    Ok(version as u64)
}

async fn persist(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &UpdateMailSettings,
    version: i64,
) -> Result<(), SubscriberApplyError> {
    let view = command.settings.view();
    let policy = view.subscriptions.view();
    sqlx::query("INSERT INTO mail_settings(singleton,version,mode,operator_name,postal_address,purpose,privacy_url,contact_address,max_campaign_recipients,max_daily_messages,max_daily_confirmation_messages,send_interval_milliseconds) VALUES(1,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(singleton) DO UPDATE SET version=excluded.version,mode=excluded.mode,operator_name=excluded.operator_name,postal_address=excluded.postal_address,purpose=excluded.purpose,privacy_url=excluded.privacy_url,contact_address=excluded.contact_address,max_campaign_recipients=excluded.max_campaign_recipients,max_daily_messages=excluded.max_daily_messages,max_daily_confirmation_messages=excluded.max_daily_confirmation_messages,send_interval_milliseconds=excluded.send_interval_milliseconds")
        .bind(version).bind(mode_name(policy.mode)).bind(policy.operator_name).bind(policy.postal_address)
        .bind(policy.purpose).bind(policy.privacy_url.as_str()).bind(policy.contact_address.as_str())
        .bind(view.max_campaign_recipients as i64).bind(view.max_daily_messages as i64)
        .bind(view.max_daily_confirmation_messages as i64).bind(view.send_interval_milliseconds as i64)
        .execute(&mut **transaction).await?;
    Ok(())
}

async fn validate_activation(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &UpdateMailSettings,
) -> Result<(), SubscriberApplyError> {
    match &command.activation {
        SettingsActivation::Offline => {
            let enabled: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM mail_control_state WHERE mode='enabled')",
            )
            .fetch_one(&mut **transaction)
            .await?;
            if enabled
                || command.settings.view().subscriptions.view().mode != SubscriptionMode::Paused
            {
                return Err(SubscriberCommandError::Paused.into());
            }
        }
        SettingsActivation::Live {
            expected_control_version,
            expected_binding,
            ..
        } => {
            let control = sqlx::query("SELECT configuration_binding,control_version FROM mail_control_state WHERE singleton=1").fetch_optional(&mut **transaction).await?.ok_or(SubscriberCommandError::ControlsUnavailable)?;
            if control.try_get::<Option<&[u8]>, _>("configuration_binding")?
                != Some(expected_binding.as_slice())
            {
                return Err(SubscriberCommandError::ConfigurationChanged.into());
            }
            if u64::try_from(control.try_get::<i64, _>("control_version")?).ok()
                != Some(*expected_control_version)
            {
                return Err(SubscriberCommandError::StaleVersion.into());
            }
        }
    }
    Ok(())
}

async fn apply_activation(
    transaction: &mut Transaction<'_, Sqlite>,
    command: &UpdateMailSettings,
    executed_at: OffsetDateTime,
) -> Result<(), SubscriberApplyError> {
    let SettingsActivation::Live {
        configuration_binding,
        ..
    } = &command.activation
    else {
        return Ok(());
    };
    let view = command.settings.view();
    set_policy(
        transaction,
        SubscriberPolicy {
            configuration_binding: *configuration_binding,
            mode: match view.subscriptions.view().mode {
                SubscriptionMode::Paused => SubscriberMode::Paused,
                SubscriptionMode::Enabled => SubscriberMode::Enabled,
            },
            max_daily_messages: view.max_daily_messages,
            max_daily_confirmations: view.max_daily_confirmation_messages,
            max_campaign_recipients: view.max_campaign_recipients,
        },
        executed_at.unix_timestamp(),
    )
    .await
}
