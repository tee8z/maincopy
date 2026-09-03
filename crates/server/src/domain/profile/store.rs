use maincopy_shared::{
    auth::{AdminScope, AdminSessionId, AgentCredentialId, UserId, UserStatus},
    profile::{LightningAddress, ProfileDisplayName, ProfileVersion},
};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::database::store::{DatabaseAdmissionError, Mutation};
#[cfg(test)]
use crate::domain::auth::store::AdminMutationKey;
use crate::domain::auth::store::{
    AuditPrincipalReference, AuthApplyError, AuthCommandError, MutationAuditContext,
    append_success_audit, require_principal_scope,
};

use super::ProfilePrecondition;

const LNURL_HRP: bech32::Hrp = bech32::Hrp::parse_unchecked("lnurl");
const LNURL_PAY_PATH: &str = "/.well-known/lnurlp/";

/// Profile persistence accessed through query-only readers and the sole writer.
#[derive(Clone)]
pub(crate) struct ProfileStore {
    readers: SqlitePool,
    mutations: mpsc::Sender<Mutation>,
}

impl ProfileStore {
    pub(crate) const fn new(readers: SqlitePool, mutations: mpsc::Sender<Mutation>) -> Self {
        Self { readers, mutations }
    }

    pub(crate) async fn profile(
        &self,
        user_id: UserId,
    ) -> Result<Option<StoredUserProfile>, ProfileLoadError> {
        let row = sqlx::query_as::<_, UserProfileRow>(
            "SELECT display_name, lightning_address, tips_enabled, version, updated_at_ns \
             FROM user_profiles WHERE user_id = ?",
        )
        .bind(user_id.as_uuid().as_bytes().as_slice())
        .fetch_optional(&self.readers)
        .await?;
        row.map(|row| StoredUserProfile::try_from_row(user_id, row))
            .transpose()
    }

    pub(crate) async fn active_tip_recipient(
        &self,
    ) -> Result<StoredTipRecipientSetting, ProfileLoadError> {
        let row = sqlx::query_as::<_, TipRecipientSettingRow>(
            "SELECT recipient_user_id, version, updated_at_ns \
             FROM site_tip_recipient WHERE singleton = 1",
        )
        .fetch_optional(&self.readers)
        .await?
        .ok_or(ProfileLoadError::MissingTipRecipientSetting)?;
        StoredTipRecipientSetting::try_from(row)
    }

    pub(crate) async fn effective_tip_recipient(
        &self,
    ) -> Result<Option<TipRecipientProjection>, ProfileLoadError> {
        let mut transaction = self.readers.begin().await?;
        let setting = sqlx::query_as::<_, TipRecipientSettingRow>(
            "SELECT recipient_user_id, version, updated_at_ns \
             FROM site_tip_recipient WHERE singleton = 1",
        )
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(ProfileLoadError::MissingTipRecipientSetting)?;
        let setting = StoredTipRecipientSetting::try_from(setting)?;
        let Some(user_id) = setting.recipient_user_id else {
            transaction.commit().await?;
            return Ok(None);
        };

        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM users WHERE user_id = ?")
                .bind(user_id.as_uuid().as_bytes().as_slice())
                .fetch_optional(&mut *transaction)
                .await?;
        let status = status
            .ok_or(ProfileLoadError::MissingTipRecipientUser)
            .and_then(|status| {
                UserStatus::parse(&status).ok_or(ProfileLoadError::InvalidUserStatus)
            })?;
        let profile = sqlx::query_as::<_, UserProfileRow>(
            "SELECT display_name, lightning_address, tips_enabled, version, updated_at_ns \
             FROM user_profiles WHERE user_id = ?",
        )
        .bind(user_id.as_uuid().as_bytes().as_slice())
        .fetch_optional(&mut *transaction)
        .await?
        .map(|row| StoredUserProfile::try_from_row(user_id, row))
        .transpose()?;
        transaction.commit().await?;

        let Some(profile) = profile else {
            return Ok(None);
        };
        if status != UserStatus::Enabled || !profile.tips_enabled {
            return Ok(None);
        }
        let Some(address) = profile.lightning_address else {
            return Ok(None);
        };
        TipRecipientProjection::from_validated_profile(profile.display_name, address).map(Some)
    }

    pub(crate) async fn update_profile(
        &self,
        command: UpdateProfile,
    ) -> Result<StoredUserProfile, ProfileMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::UpdateProfile {
            command,
            respond_to,
        })?;
        receive_mutation(response).await
    }

    pub(crate) async fn set_tip_recipient(
        &self,
        command: SetTipRecipient,
    ) -> Result<StoredTipRecipientSetting, ProfileMutationError> {
        let (respond_to, response) = oneshot::channel();
        self.admit(Mutation::SetTipRecipient {
            command,
            respond_to,
        })?;
        receive_mutation(response).await
    }

    fn admit(&self, mutation: Mutation) -> Result<(), ProfileMutationError> {
        self.mutations
            .try_send(mutation)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => DatabaseAdmissionError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => DatabaseAdmissionError::WriterClosed,
            })?;
        Ok(())
    }
}

async fn receive_mutation<Output>(
    response: oneshot::Receiver<Result<Output, ProfileCommandError>>,
) -> Result<Output, ProfileMutationError> {
    response
        .await
        .map_err(|_| ProfileMutationError::Command(ProfileCommandError::OutcomeUnknown))?
        .map_err(ProfileMutationError::Command)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredUserProfile {
    pub(crate) user_id: UserId,
    pub(crate) display_name: Option<ProfileDisplayName>,
    pub(crate) lightning_address: Option<LightningAddress>,
    pub(crate) tips_enabled: bool,
    pub(crate) version: ProfileVersion,
    pub(crate) updated_at: OffsetDateTime,
}

impl StoredUserProfile {
    fn try_from_row(user_id: UserId, row: UserProfileRow) -> Result<Self, ProfileLoadError> {
        Ok(Self {
            user_id,
            display_name: row
                .display_name
                .map(|value| {
                    ProfileDisplayName::parse(&value)
                        .map_err(|_| ProfileLoadError::InvalidDisplayName)
                })
                .transpose()?,
            lightning_address: row
                .lightning_address
                .map(|value| {
                    LightningAddress::parse(&value)
                        .map_err(|_| ProfileLoadError::InvalidLightningAddress)
                })
                .transpose()?,
            tips_enabled: boolean(row.tips_enabled)?,
            version: positive_version(row.version)?,
            updated_at: timestamp(row.updated_at_ns)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredTipRecipientSetting {
    pub(crate) recipient_user_id: Option<UserId>,
    pub(crate) version: ProfileVersion,
    pub(crate) updated_at: OffsetDateTime,
}

impl TryFrom<TipRecipientSettingRow> for StoredTipRecipientSetting {
    type Error = ProfileLoadError;

    fn try_from(row: TipRecipientSettingRow) -> Result<Self, Self::Error> {
        Ok(Self {
            recipient_user_id: row
                .recipient_user_id
                .map(|value| user_id(&value))
                .transpose()?,
            version: positive_version(row.version)?,
            updated_at: timestamp(row.updated_at_ns)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TipRecipientProjection {
    display_name: Option<ProfileDisplayName>,
    address: LightningAddress,
    lnurl: Box<str>,
    wallet_link: Box<str>,
}

impl TipRecipientProjection {
    pub(crate) fn from_validated_profile(
        display_name: Option<ProfileDisplayName>,
        address: LightningAddress,
    ) -> Result<Self, ProfileLoadError> {
        let endpoint = format!(
            "https://{}{LNURL_PAY_PATH}{}",
            address.as_domain(),
            address.as_username()
        );
        let lnurl = bech32::encode_upper::<bech32::Bech32>(LNURL_HRP, endpoint.as_bytes())
            .map_err(ProfileLoadError::LnurlEncoding)?
            .into_boxed_str();
        let wallet_link = format!("lightning:{lnurl}").into_boxed_str();
        Ok(Self {
            display_name,
            address,
            lnurl,
            wallet_link,
        })
    }

    pub(crate) fn as_view(&self) -> TipRecipientView<'_> {
        TipRecipientView {
            display_name: self.display_name.as_ref().map(ProfileDisplayName::as_str),
            address: self.address.as_str(),
            lnurl: &self.lnurl,
            wallet_link: &self.wallet_link,
        }
    }

    /// Stable length-framed input for preview presentation identities.
    pub(crate) fn identity_bytes(&self) -> Vec<u8> {
        let view = self.as_view();
        let mut bytes = b"maincopy.tip-recipient.v1\0".to_vec();
        append_optional_identity_field(&mut bytes, view.display_name);
        append_identity_field(&mut bytes, view.address);
        append_identity_field(&mut bytes, view.lnurl);
        append_identity_field(&mut bytes, view.wallet_link);
        bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TipRecipientView<'a> {
    pub(crate) display_name: Option<&'a str>,
    pub(crate) address: &'a str,
    pub(crate) lnurl: &'a str,
    pub(crate) wallet_link: &'a str,
}

fn append_optional_identity_field(bytes: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            bytes.push(1);
            append_identity_field(bytes, value);
        }
        None => bytes.push(0),
    }
}

fn append_identity_field(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpdateProfile {
    pub(crate) user_id: UserId,
    pub(crate) precondition: ProfilePrecondition,
    pub(crate) display_name: Option<ProfileDisplayName>,
    pub(crate) lightning_address: Option<LightningAddress>,
    pub(crate) tips_enabled: bool,
    pub(crate) occurred_at: OffsetDateTime,
    pub(crate) audit: MutationAuditContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SetTipRecipient {
    pub(crate) expected_version: ProfileVersion,
    pub(crate) recipient_user_id: Option<UserId>,
    pub(crate) occurred_at: OffsetDateTime,
    pub(crate) audit: MutationAuditContext,
}

impl UpdateProfile {
    fn fingerprint(&self) -> ProfileCommandFingerprint {
        let mut builder = ProfileFingerprintBuilder::new(ProfileMutationAction::PutUserProfile);
        builder.uuid(self.user_id.as_uuid());
        match self.precondition {
            ProfilePrecondition::Create => builder.field(b"create"),
            ProfilePrecondition::Replace(version) => {
                builder.field(b"replace");
                builder.version(version);
            }
        }
        builder.optional_field(
            self.display_name
                .as_ref()
                .map(|value| value.as_str().as_bytes()),
        );
        builder.optional_field(
            self.lightning_address
                .as_ref()
                .map(|value| value.as_str().as_bytes()),
        );
        builder.boolean(self.tips_enabled);
        builder.finish()
    }
}

impl SetTipRecipient {
    fn fingerprint(&self) -> ProfileCommandFingerprint {
        let mut builder =
            ProfileFingerprintBuilder::new(ProfileMutationAction::ReplaceTipRecipient);
        builder.version(self.expected_version);
        builder.optional_field(
            self.recipient_user_id
                .as_ref()
                .map(|value| value.as_uuid().as_bytes().as_slice()),
        );
        builder.finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProfileMutationAction {
    PutUserProfile,
    ReplaceTipRecipient,
}

impl ProfileMutationAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::PutUserProfile => "profile.user.put",
            Self::ReplaceTipRecipient => "profile.tip-recipient.replace",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProfileCommandFingerprint([u8; 32]);

impl ProfileCommandFingerprint {
    const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

struct ProfileFingerprintBuilder(blake3::Hasher);

impl ProfileFingerprintBuilder {
    fn new(action: ProfileMutationAction) -> Self {
        let mut builder = Self(blake3::Hasher::new());
        builder.field(action.as_str().as_bytes());
        builder
    }

    fn field(&mut self, value: &[u8]) {
        self.0.update(&(value.len() as u64).to_be_bytes());
        self.0.update(value);
    }

    fn optional_field(&mut self, value: Option<&[u8]>) {
        match value {
            Some(value) => {
                self.field(b"some");
                self.field(value);
            }
            None => self.field(b"none"),
        }
    }

    fn uuid(&mut self, value: &Uuid) {
        self.field(value.as_bytes());
    }

    fn version(&mut self, value: ProfileVersion) {
        self.field(&value.into_u64().to_be_bytes());
    }

    fn boolean(&mut self, value: bool) {
        self.field(&[u8::from(value)]);
    }

    fn finish(self) -> ProfileCommandFingerprint {
        ProfileCommandFingerprint(*self.0.finalize().as_bytes())
    }
}

pub(crate) type UpdateProfileResult = Result<StoredUserProfile, ProfileCommandError>;
pub(crate) type SetTipRecipientResult = Result<StoredTipRecipientSetting, ProfileCommandError>;

pub(crate) async fn apply_update_profile(
    transaction: &mut Transaction<'_, Sqlite>,
    command: UpdateProfile,
) -> Result<StoredUserProfile, ProfileApplyError> {
    let action = ProfileMutationAction::PutUserProfile;
    let fingerprint = command.fingerprint();
    if let Some(result) =
        replay_profile_update(transaction, &command.audit, action, fingerprint).await?
    {
        return Ok(result);
    }
    require_profile_actor(&command.audit, command.user_id)?;
    map_auth_apply(
        require_principal_scope(
            transaction,
            &command.audit.principal,
            AdminScope::ProfileManage,
            command.occurred_at,
        )
        .await,
    )?;

    let current: Option<(i64, i64)> =
        sqlx::query_as("SELECT version, updated_at_ns FROM user_profiles WHERE user_id = ?")
            .bind(command.user_id.as_uuid().as_bytes().as_slice())
            .fetch_optional(&mut **transaction)
            .await?;
    let version = match (current, command.precondition) {
        (None, ProfilePrecondition::Create) => {
            ProfileVersion::new(1).map_err(|_| ProfileCommandError::InvalidValue)?
        }
        (Some(_), ProfilePrecondition::Create) => {
            return Err(ProfileCommandError::Conflict.into());
        }
        (None, ProfilePrecondition::Replace(_)) => {
            return Err(ProfileCommandError::NotFound.into());
        }
        (Some((current, updated_at_ns)), ProfilePrecondition::Replace(expected)) => {
            let current_version =
                positive_version(current).map_err(|_| ProfileApplyError::CorruptStoredState)?;
            require_version(current_version, expected)?;
            let updated_at =
                timestamp(updated_at_ns).map_err(|_| ProfileApplyError::CorruptStoredState)?;
            if command.occurred_at < updated_at {
                return Err(ProfileCommandError::InvalidValue.into());
            }
            checked_next_version(current_version)?
        }
    };
    let updated_at_ns = command_timestamp(command.occurred_at)?;
    let version_i64 = i64::from(version);
    let display_name = command
        .display_name
        .as_ref()
        .map(ProfileDisplayName::as_str);
    let lightning_address = command
        .lightning_address
        .as_ref()
        .map(LightningAddress::as_str);

    match current {
        None => {
            sqlx::query(
                "INSERT INTO user_profiles (\
                    user_id, display_name, lightning_address, tips_enabled, version, updated_at_ns\
                 ) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(command.user_id.as_uuid().as_bytes().as_slice())
            .bind(display_name)
            .bind(lightning_address)
            .bind(command.tips_enabled)
            .bind(version_i64)
            .bind(updated_at_ns)
            .execute(&mut **transaction)
            .await?;
        }
        Some((current, _)) => {
            let result = sqlx::query(
                "UPDATE user_profiles SET \
                    display_name = ?, lightning_address = ?, tips_enabled = ?, \
                    version = ?, updated_at_ns = ? \
                 WHERE user_id = ? AND version = ?",
            )
            .bind(display_name)
            .bind(lightning_address)
            .bind(command.tips_enabled)
            .bind(version_i64)
            .bind(updated_at_ns)
            .bind(command.user_id.as_uuid().as_bytes().as_slice())
            .bind(current)
            .execute(&mut **transaction)
            .await?;
            require_one_row(result.rows_affected())?;
        }
    }

    let result = StoredUserProfile {
        user_id: command.user_id,
        display_name: command.display_name,
        lightning_address: command.lightning_address,
        tips_enabled: command.tips_enabled,
        version,
        updated_at: command.occurred_at.to_offset(UtcOffset::UTC),
    };
    complete_profile_update(
        transaction,
        &command.audit,
        command.occurred_at,
        action,
        fingerprint,
        &result,
    )
    .await?;
    Ok(result)
}

pub(crate) async fn apply_set_tip_recipient(
    transaction: &mut Transaction<'_, Sqlite>,
    command: SetTipRecipient,
) -> Result<StoredTipRecipientSetting, ProfileApplyError> {
    let action = ProfileMutationAction::ReplaceTipRecipient;
    let fingerprint = command.fingerprint();
    if let Some(result) =
        replay_tip_recipient_update(transaction, &command.audit, action, fingerprint).await?
    {
        return Ok(result);
    }
    require_runtime_actor(&command.audit)?;
    map_auth_apply(
        require_principal_scope(
            transaction,
            &command.audit.principal,
            AdminScope::LightningManage,
            command.occurred_at,
        )
        .await,
    )?;

    if let Some(user_id) = command.recipient_user_id {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE user_id = ?)")
                .bind(user_id.as_uuid().as_bytes().as_slice())
                .fetch_one(&mut **transaction)
                .await?;
        if !exists {
            return Err(ProfileCommandError::NotFound.into());
        }
    }

    let current: Option<(i64, i64)> =
        sqlx::query_as("SELECT version, updated_at_ns FROM site_tip_recipient WHERE singleton = 1")
            .fetch_optional(&mut **transaction)
            .await?;
    let (current, current_updated_at_ns) = current.ok_or(ProfileApplyError::CorruptStoredState)?;
    let current_version =
        positive_version(current).map_err(|_| ProfileApplyError::CorruptStoredState)?;
    require_version(current_version, command.expected_version)?;
    let current_updated_at =
        timestamp(current_updated_at_ns).map_err(|_| ProfileApplyError::CorruptStoredState)?;
    if command.occurred_at < current_updated_at {
        return Err(ProfileCommandError::InvalidValue.into());
    }
    let version = checked_next_version(current_version)?;
    let updated_at_ns = command_timestamp(command.occurred_at)?;
    let result = sqlx::query(
        "UPDATE site_tip_recipient SET recipient_user_id = ?, version = ?, updated_at_ns = ? \
         WHERE singleton = 1 AND version = ?",
    )
    .bind(
        command
            .recipient_user_id
            .map(|user_id| user_id.into_uuid().into_bytes().to_vec()),
    )
    .bind(i64::from(version))
    .bind(updated_at_ns)
    .bind(current)
    .execute(&mut **transaction)
    .await?;
    require_one_row(result.rows_affected())?;

    let result = StoredTipRecipientSetting {
        recipient_user_id: command.recipient_user_id,
        version,
        updated_at: command.occurred_at.to_offset(UtcOffset::UTC),
    };
    complete_tip_recipient_update(
        transaction,
        &command.audit,
        command.occurred_at,
        action,
        fingerprint,
        &result,
    )
    .await?;
    Ok(result)
}

async fn replay_profile_update(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
    action: ProfileMutationAction,
    fingerprint: ProfileCommandFingerprint,
) -> Result<Option<StoredUserProfile>, ProfileApplyError> {
    let Some(row) = load_profile_receipt(transaction, audit).await? else {
        return Ok(None);
    };
    validate_receipt_binding(&row, audit, action, fingerprint)?;
    row.into_profile().map(Some)
}

async fn replay_tip_recipient_update(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
    action: ProfileMutationAction,
    fingerprint: ProfileCommandFingerprint,
) -> Result<Option<StoredTipRecipientSetting>, ProfileApplyError> {
    let Some(row) = load_profile_receipt(transaction, audit).await? else {
        return Ok(None);
    };
    validate_receipt_binding(&row, audit, action, fingerprint)?;
    row.into_tip_recipient().map(Some)
}

async fn load_profile_receipt(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
) -> Result<Option<ProfileMutationReceiptRow>, ProfileApplyError> {
    Ok(sqlx::query_as::<_, ProfileMutationReceiptRow>(
        "SELECT receipt.idempotency_key AS receipt_idempotency_key, \
                receipt.command_fingerprint, receipt.result_kind, receipt.profile_user_id, \
                receipt.display_name, receipt.lightning_address, receipt.tips_enabled, \
                receipt.recipient_user_id, receipt.result_version, \
                receipt.result_updated_at_ns, audit.principal_kind, audit.actor_user_id, \
                audit.session_id, audit.agent_credential_id, audit.action, audit.outcome, \
                audit.reason_code \
         FROM admin_audit_events AS audit \
         LEFT JOIN admin_profile_mutation_receipts AS receipt \
           ON receipt.audit_event_id = audit.audit_event_id \
         WHERE audit.idempotency_key = ?",
    )
    .bind(audit.idempotency_key.0.as_bytes().as_slice())
    .fetch_optional(&mut **transaction)
    .await?)
}

fn validate_receipt_binding(
    row: &ProfileMutationReceiptRow,
    audit: &MutationAuditContext,
    action: ProfileMutationAction,
    fingerprint: ProfileCommandFingerprint,
) -> Result<(), ProfileApplyError> {
    let receipt_idempotency_key = row
        .receipt_idempotency_key
        .as_deref()
        .ok_or(ProfileCommandError::IdempotencyConflict)?;
    if receipt_idempotency_key != audit.idempotency_key.0.as_bytes() {
        return Err(ProfileApplyError::CorruptStoredState);
    }
    let principal = receipt_principal(row)?;
    if row.action != action.as_str() || principal != audit.principal {
        return Err(ProfileCommandError::IdempotencyConflict.into());
    }
    if row.outcome != "succeeded" || row.reason_code.is_some() {
        return Err(ProfileApplyError::CorruptStoredState);
    }
    let stored_fingerprint = row
        .command_fingerprint
        .as_deref()
        .ok_or(ProfileApplyError::CorruptStoredState)?;
    if stored_fingerprint.len() != fingerprint.as_bytes().len() {
        return Err(ProfileApplyError::CorruptStoredState);
    }
    if stored_fingerprint != fingerprint.as_bytes() {
        return Err(ProfileCommandError::IdempotencyConflict.into());
    }
    Ok(())
}

async fn complete_profile_update(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
    occurred_at: OffsetDateTime,
    action: ProfileMutationAction,
    fingerprint: ProfileCommandFingerprint,
    result: &StoredUserProfile,
) -> Result<(), ProfileApplyError> {
    map_auth_apply(append_success_audit(transaction, audit, occurred_at, action.as_str()).await)?;
    let insertion = sqlx::query(
        "INSERT INTO admin_profile_mutation_receipts (\
            idempotency_key, audit_event_id, command_fingerprint, result_kind, profile_user_id, \
            display_name, lightning_address, tips_enabled, recipient_user_id, result_version, \
            result_updated_at_ns\
         ) VALUES (?, ?, ?, 'user_profile', ?, ?, ?, ?, NULL, ?, ?)",
    )
    .bind(audit.idempotency_key.0.as_bytes().as_slice())
    .bind(audit.audit_event_id.as_uuid().as_bytes().as_slice())
    .bind(fingerprint.as_bytes().as_slice())
    .bind(result.user_id.as_uuid().as_bytes().as_slice())
    .bind(result.display_name.as_ref().map(ProfileDisplayName::as_str))
    .bind(
        result
            .lightning_address
            .as_ref()
            .map(LightningAddress::as_str),
    )
    .bind(result.tips_enabled)
    .bind(i64::from(result.version))
    .bind(command_timestamp(result.updated_at)?)
    .execute(&mut **transaction)
    .await;
    map_receipt_insertion(insertion)
}

async fn complete_tip_recipient_update(
    transaction: &mut Transaction<'_, Sqlite>,
    audit: &MutationAuditContext,
    occurred_at: OffsetDateTime,
    action: ProfileMutationAction,
    fingerprint: ProfileCommandFingerprint,
    result: &StoredTipRecipientSetting,
) -> Result<(), ProfileApplyError> {
    map_auth_apply(append_success_audit(transaction, audit, occurred_at, action.as_str()).await)?;
    let insertion = sqlx::query(
        "INSERT INTO admin_profile_mutation_receipts (\
            idempotency_key, audit_event_id, command_fingerprint, result_kind, profile_user_id, \
            display_name, lightning_address, tips_enabled, recipient_user_id, result_version, \
            result_updated_at_ns\
         ) VALUES (?, ?, ?, 'tip_recipient', NULL, NULL, NULL, NULL, ?, ?, ?)",
    )
    .bind(audit.idempotency_key.0.as_bytes().as_slice())
    .bind(audit.audit_event_id.as_uuid().as_bytes().as_slice())
    .bind(fingerprint.as_bytes().as_slice())
    .bind(
        result
            .recipient_user_id
            .map(|user_id| user_id.into_uuid().into_bytes().to_vec()),
    )
    .bind(i64::from(result.version))
    .bind(command_timestamp(result.updated_at)?)
    .execute(&mut **transaction)
    .await;
    map_receipt_insertion(insertion)
}

fn map_receipt_insertion(
    insertion: Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error>,
) -> Result<(), ProfileApplyError> {
    match insertion {
        Ok(_) => Ok(()),
        Err(error) if is_unique_violation(&error) => {
            Err(ProfileCommandError::IdempotencyConflict.into())
        }
        Err(error) => Err(error.into()),
    }
}

fn require_profile_actor(
    audit: &MutationAuditContext,
    profile_user_id: UserId,
) -> Result<(), ProfileApplyError> {
    if require_runtime_actor(audit)? == profile_user_id {
        Ok(())
    } else {
        Err(ProfileCommandError::Forbidden.into())
    }
}

fn require_runtime_actor(audit: &MutationAuditContext) -> Result<UserId, ProfileApplyError> {
    match audit.principal {
        AuditPrincipalReference::BrowserSession { user_id, .. }
        | AuditPrincipalReference::AgentCredential { user_id, .. } => Ok(user_id),
        AuditPrincipalReference::Offline { .. } | AuditPrincipalReference::Unauthenticated => {
            Err(ProfileCommandError::Forbidden.into())
        }
    }
}

fn receipt_principal(
    row: &ProfileMutationReceiptRow,
) -> Result<AuditPrincipalReference, ProfileApplyError> {
    let actor = row
        .actor_user_id
        .as_deref()
        .map(user_id)
        .transpose()
        .map_err(|_| ProfileApplyError::CorruptStoredState)?;
    let session = row
        .session_id
        .as_deref()
        .map(admin_session_id)
        .transpose()?;
    let agent = row
        .agent_credential_id
        .as_deref()
        .map(agent_credential_id)
        .transpose()?;
    match (row.principal_kind.as_str(), actor, session, agent) {
        ("browser_session", Some(user_id), Some(session_id), None) => {
            Ok(AuditPrincipalReference::BrowserSession {
                user_id,
                session_id,
            })
        }
        ("agent_credential", Some(user_id), None, Some(credential_id)) => {
            Ok(AuditPrincipalReference::AgentCredential {
                user_id,
                credential_id,
            })
        }
        ("offline", user_id, None, None) => Ok(AuditPrincipalReference::Offline { user_id }),
        ("unauthenticated", None, None, None) => Ok(AuditPrincipalReference::Unauthenticated),
        _ => Err(ProfileApplyError::CorruptStoredState),
    }
}

fn admin_session_id(value: &[u8]) -> Result<AdminSessionId, ProfileApplyError> {
    Uuid::from_slice(value)
        .map(AdminSessionId::from_uuid)
        .map_err(|_| ProfileApplyError::CorruptStoredState)
}

fn agent_credential_id(value: &[u8]) -> Result<AgentCredentialId, ProfileApplyError> {
    Uuid::from_slice(value)
        .map(AgentCredentialId::from_uuid)
        .map_err(|_| ProfileApplyError::CorruptStoredState)
}

fn map_auth_apply<T>(result: Result<T, AuthApplyError>) -> Result<T, ProfileApplyError> {
    result.map_err(|error| match error {
        AuthApplyError::Operation(source) => ProfileApplyError::Operation(source),
        AuthApplyError::CorruptStoredState => ProfileApplyError::CorruptStoredState,
        AuthApplyError::Command(
            AuthCommandError::Conflict | AuthCommandError::IdempotencyConflict,
        ) => ProfileCommandError::IdempotencyConflict.into(),
        AuthApplyError::Command(AuthCommandError::NotFound | AuthCommandError::ScopeEscalation) => {
            ProfileCommandError::Forbidden.into()
        }
        AuthApplyError::Command(AuthCommandError::InvalidValue) => {
            ProfileCommandError::InvalidValue.into()
        }
        AuthApplyError::Command(
            AuthCommandError::AlreadyBootstrapped
            | AuthCommandError::BootstrapRequired
            | AuthCommandError::StaleVersion
            | AuthCommandError::NoLoginProvider
            | AuthCommandError::EnabledUserRequiresCredential
            | AuthCommandError::LastEnabledOwner
            | AuthCommandError::InvalidChallenge
            | AuthCommandError::ChallengeCapacity
            | AuthCommandError::ReplayCapacity
            | AuthCommandError::SessionCapacity
            | AuthCommandError::AgentCredentialCapacity
            | AuthCommandError::ReplayedProof
            | AuthCommandError::OutcomeUnknown,
        ) => ProfileApplyError::CorruptStoredState,
    })
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
}

fn require_version(
    actual: ProfileVersion,
    expected: ProfileVersion,
) -> Result<(), ProfileApplyError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ProfileCommandError::StaleVersion.into())
    }
}

fn require_one_row(rows: u64) -> Result<(), ProfileApplyError> {
    if rows == 1 {
        Ok(())
    } else {
        Err(ProfileCommandError::StaleVersion.into())
    }
}

fn checked_next_version(version: ProfileVersion) -> Result<ProfileVersion, ProfileApplyError> {
    version
        .into_u64()
        .checked_add(1)
        .and_then(|version| ProfileVersion::new(version).ok())
        .ok_or_else(|| ProfileCommandError::InvalidValue.into())
}

fn command_timestamp(value: OffsetDateTime) -> Result<i64, ProfileApplyError> {
    i64::try_from(value.unix_timestamp_nanos())
        .map_err(|_| ProfileCommandError::InvalidValue.into())
}

fn user_id(value: &[u8]) -> Result<UserId, ProfileLoadError> {
    Uuid::from_slice(value)
        .map(UserId::from_uuid)
        .map_err(|_| ProfileLoadError::InvalidUserId)
}

fn stored_user_id(value: &[u8]) -> Result<UserId, ProfileApplyError> {
    user_id(value).map_err(|_| ProfileApplyError::CorruptStoredState)
}

fn boolean(value: i64) -> Result<bool, ProfileLoadError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(ProfileLoadError::InvalidBoolean),
    }
}

fn positive_version(value: i64) -> Result<ProfileVersion, ProfileLoadError> {
    u64::try_from(value)
        .ok()
        .and_then(|value| ProfileVersion::new(value).ok())
        .ok_or(ProfileLoadError::InvalidVersion)
}

fn timestamp(value: i64) -> Result<OffsetDateTime, ProfileLoadError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(value))
        .map(|timestamp| timestamp.to_offset(UtcOffset::UTC))
        .map_err(|_| ProfileLoadError::InvalidTimestamp)
}

#[derive(FromRow)]
struct UserProfileRow {
    display_name: Option<String>,
    lightning_address: Option<String>,
    tips_enabled: i64,
    version: i64,
    updated_at_ns: i64,
}

#[derive(FromRow)]
struct TipRecipientSettingRow {
    recipient_user_id: Option<Vec<u8>>,
    version: i64,
    updated_at_ns: i64,
}

#[derive(FromRow)]
struct ProfileMutationReceiptRow {
    receipt_idempotency_key: Option<Vec<u8>>,
    command_fingerprint: Option<Vec<u8>>,
    result_kind: Option<String>,
    profile_user_id: Option<Vec<u8>>,
    display_name: Option<String>,
    lightning_address: Option<String>,
    tips_enabled: Option<i64>,
    recipient_user_id: Option<Vec<u8>>,
    result_version: Option<i64>,
    result_updated_at_ns: Option<i64>,
    principal_kind: String,
    actor_user_id: Option<Vec<u8>>,
    session_id: Option<Vec<u8>>,
    agent_credential_id: Option<Vec<u8>>,
    action: String,
    outcome: String,
    reason_code: Option<String>,
}

impl ProfileMutationReceiptRow {
    fn into_profile(self) -> Result<StoredUserProfile, ProfileApplyError> {
        if self.result_kind.as_deref() != Some("user_profile") || self.recipient_user_id.is_some() {
            return Err(ProfileApplyError::CorruptStoredState);
        }
        let user_id = self
            .profile_user_id
            .as_deref()
            .ok_or(ProfileApplyError::CorruptStoredState)
            .and_then(stored_user_id)?;
        let tips_enabled = self
            .tips_enabled
            .ok_or(ProfileApplyError::CorruptStoredState)?;
        let version = self
            .result_version
            .ok_or(ProfileApplyError::CorruptStoredState)?;
        let updated_at_ns = self
            .result_updated_at_ns
            .ok_or(ProfileApplyError::CorruptStoredState)?;
        StoredUserProfile::try_from_row(
            user_id,
            UserProfileRow {
                display_name: self.display_name,
                lightning_address: self.lightning_address,
                tips_enabled,
                version,
                updated_at_ns,
            },
        )
        .map_err(|_| ProfileApplyError::CorruptStoredState)
    }

    fn into_tip_recipient(self) -> Result<StoredTipRecipientSetting, ProfileApplyError> {
        if self.result_kind.as_deref() != Some("tip_recipient")
            || self.profile_user_id.is_some()
            || self.display_name.is_some()
            || self.lightning_address.is_some()
            || self.tips_enabled.is_some()
        {
            return Err(ProfileApplyError::CorruptStoredState);
        }
        let recipient_user_id = self
            .recipient_user_id
            .as_deref()
            .map(stored_user_id)
            .transpose()?;
        let version = self
            .result_version
            .ok_or(ProfileApplyError::CorruptStoredState)
            .and_then(|value| {
                positive_version(value).map_err(|_| ProfileApplyError::CorruptStoredState)
            })?;
        let updated_at = self
            .result_updated_at_ns
            .ok_or(ProfileApplyError::CorruptStoredState)
            .and_then(|value| {
                timestamp(value).map_err(|_| ProfileApplyError::CorruptStoredState)
            })?;
        Ok(StoredTipRecipientSetting {
            recipient_user_id,
            version,
            updated_at,
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum ProfileLoadError {
    #[error("could not query profile state")]
    Query(#[from] sqlx::Error),
    #[error("the active tip-recipient setting is missing")]
    MissingTipRecipientSetting,
    #[error("the selected tip-recipient user is missing")]
    MissingTipRecipientUser,
    #[error("a stored profile user identifier is invalid")]
    InvalidUserId,
    #[error("a stored user status is invalid")]
    InvalidUserStatus,
    #[error("a stored profile display name is invalid")]
    InvalidDisplayName,
    #[error("a stored Lightning Address is invalid")]
    InvalidLightningAddress,
    #[error("a stored profile boolean is invalid")]
    InvalidBoolean,
    #[error("a stored profile resource version is invalid")]
    InvalidVersion,
    #[error("a stored profile timestamp is invalid")]
    InvalidTimestamp,
    #[error("the LNURL handoff could not be encoded: {0}")]
    LnurlEncoding(#[source] bech32::EncodeError),
}

#[derive(Debug, Error)]
pub(crate) enum ProfileMutationError {
    #[error(transparent)]
    Admission(#[from] DatabaseAdmissionError),
    #[error(transparent)]
    Command(#[from] ProfileCommandError),
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ProfileCommandError {
    #[error("the profile resource does not exist")]
    NotFound,
    #[error("the profile resource already exists")]
    Conflict,
    #[error("the profile command actor lacks required authority")]
    Forbidden,
    #[error("the profile resource version is stale")]
    StaleVersion,
    #[error("the idempotency key is already bound to a different admin command")]
    IdempotencyConflict,
    #[error("the profile command contains a value outside the persistence range")]
    InvalidValue,
    #[error("the profile command outcome is unknown")]
    OutcomeUnknown,
}

#[derive(Debug, Error)]
pub(crate) enum ProfileApplyError {
    #[error(transparent)]
    Command(#[from] ProfileCommandError),
    #[error("profile persistence operation failed")]
    Operation(#[from] sqlx::Error),
    #[error("stored profile state is invalid")]
    CorruptStoredState,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        path::{Path, PathBuf},
        process::Command,
    };

    use bech32::{Bech32, primitives::decode::CheckedHrpstring};
    use maincopy_shared::auth::{AdminAuditEventId, InstanceId, UserRole};
    use sqlx::{Connection as _, SqliteConnection};
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        config::{
            DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
            DatabaseWriterQueueCapacity,
        },
        database::{self, store::DatabaseStore},
        domain::auth::{NostrPublicKey, store as auth_store},
    };

    const OWNER_KEY: &str = "63fe6318dc58583cfe16810f86dd09e18bfd76aabc24a0081ce2856f330504ed";
    const OTHER_USER_KEY: &str = "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5";
    const PROFILE_AGENT_KEY: &str =
        "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9";
    const SECOND_PROFILE_AGENT_KEY: &str =
        "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    const OTHER_USER_AGENT_KEY: &str =
        "e493dbf1c10d80f3581e4904930b1404cc6c13900ee0758474fa94abe8c4cd13";
    const ALICE_ENDPOINT: &str = "https://example.com/.well-known/lnurlp/alice";
    const ALICE_LNURL: &str =
        "LNURL1DP68GURN8GHJ7ETCV9KHQMR99E3K7MF09EMK2MRV944KUMMHDCHKCMN4WFK8QTMPD35KXEG9SAEVQ";
    const PROFILE_CRASH_DATABASE_ENV: &str = "MAINCOPY_TEST_PROFILE_CRASH_DATABASE";
    const WRITER_CRASH_POINT_ENV: &str = "MAINCOPY_TEST_WRITER_CRASH_POINT";
    const PROFILE_CRASH_HELPER_TEST: &str =
        "domain::profile::store::tests::profile_update_crash_process";

    struct Harness {
        root: tempfile::TempDir,
        store: DatabaseStore,
        shutdown: CancellationToken,
        writer: JoinHandle<()>,
        owner: UserId,
    }

    impl Harness {
        async fn start() -> Self {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("state/maincopy.db");
            let database = database::bootstrap(configuration(&path)).await.unwrap();
            let (store, writer) = database.into_store(16);
            let shutdown = CancellationToken::new();
            let task_shutdown = shutdown.clone();
            let writer = tokio::spawn(async move {
                writer.run(task_shutdown).await.unwrap();
            });
            let owner = user(2);
            store
                .auth
                .bootstrap_identity(auth_store::BootstrapIdentity {
                    instance_id: InstanceId::from_uuid(uuid(1)),
                    owner_user_id: owner,
                    credential: auth_store::NewHumanCredential::Nostr {
                        public_key: NostrPublicKey::parse(OWNER_KEY).unwrap(),
                    },
                    configured_providers: auth_store::ConfiguredLoginProviders::new(false, true)
                        .unwrap(),
                    occurred_at: at(10),
                    audit_event_id: AdminAuditEventId::from_uuid(uuid(3)),
                })
                .await
                .unwrap();
            register_agent(
                &store,
                owner,
                profile_agent(owner),
                PROFILE_AGENT_KEY,
                BTreeSet::from(AdminScope::ALL),
                at(11),
                bootstrap_registration_audit(owner),
            )
            .await;
            register_agent(
                &store,
                owner,
                second_profile_agent(owner),
                SECOND_PROFILE_AGENT_KEY,
                BTreeSet::from(AdminScope::ALL),
                at(12),
                profile_audit(owner, 900),
            )
            .await;
            Self {
                root,
                store,
                shutdown,
                writer,
                owner,
            }
        }

        async fn restart(self) -> Self {
            let Self {
                root,
                store,
                shutdown,
                writer,
                owner,
            } = self;
            drop(store);
            shutdown.cancel();
            writer.await.unwrap();

            let path = root.path().join("state/maincopy.db");
            let database = database::bootstrap(configuration(&path)).await.unwrap();
            let (store, writer) = database.into_store(16);
            let shutdown = CancellationToken::new();
            let task_shutdown = shutdown.clone();
            let writer = tokio::spawn(async move {
                writer.run(task_shutdown).await.unwrap();
            });
            Self {
                root,
                store,
                shutdown,
                writer,
                owner,
            }
        }

        async fn stop(self) {
            let Self {
                root: _root,
                store,
                shutdown,
                writer,
                owner: _,
            } = self;
            drop(store);
            shutdown.cancel();
            writer.await.unwrap();
        }

        async fn stop_and_open_writer(self) -> (tempfile::TempDir, SqliteConnection, UserId) {
            let Self {
                root,
                store,
                shutdown,
                writer,
                owner,
            } = self;
            drop(store);
            shutdown.cancel();
            writer.await.unwrap();
            let path = root.path().join("state/maincopy.db");
            let connection = SqliteConnection::connect(path.to_str().unwrap())
                .await
                .unwrap();
            (root, connection, owner)
        }
    }

    fn configuration(path: &Path) -> DatabaseConfigurationView<'_> {
        DatabaseConfigurationView {
            path,
            busy_timeout: DatabaseBusyTimeout::from_milliseconds(1_000).unwrap(),
            writer_queue_capacity: DatabaseWriterQueueCapacity::new(16).unwrap(),
            read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
        }
    }

    fn uuid(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn user(value: u128) -> UserId {
        UserId::from_uuid(uuid(value))
    }

    fn profile_agent(user_id: UserId) -> AgentCredentialId {
        AgentCredentialId::from_uuid(uuid(1_000_000 + user_id.as_uuid().as_u128()))
    }

    fn second_profile_agent(user_id: UserId) -> AgentCredentialId {
        AgentCredentialId::from_uuid(uuid(2_000_000 + user_id.as_uuid().as_u128()))
    }

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    fn version(value: u64) -> ProfileVersion {
        ProfileVersion::new(value).unwrap()
    }

    fn profile_audit(actor: UserId, discriminator: u128) -> MutationAuditContext {
        MutationAuditContext {
            audit_event_id: AdminAuditEventId::from_uuid(uuid(10_000 + discriminator)),
            principal: AuditPrincipalReference::AgentCredential {
                user_id: actor,
                credential_id: profile_agent(actor),
            },
            request_id: Some(uuid(30_000 + discriminator)),
            idempotency_key: AdminMutationKey(uuid(40_000 + discriminator)),
        }
    }

    fn bootstrap_registration_audit(actor: UserId) -> MutationAuditContext {
        MutationAuditContext {
            audit_event_id: AdminAuditEventId::from_uuid(uuid(901)),
            principal: AuditPrincipalReference::Offline {
                user_id: Some(actor),
            },
            request_id: Some(uuid(903)),
            idempotency_key: AdminMutationKey(uuid(904)),
        }
    }

    async fn register_agent(
        store: &DatabaseStore,
        owner_user_id: UserId,
        credential_id: AgentCredentialId,
        public_key: &str,
        scopes: BTreeSet<AdminScope>,
        created_at: OffsetDateTime,
        audit: MutationAuditContext,
    ) {
        let issuer_user_id = match audit.principal {
            AuditPrincipalReference::BrowserSession { user_id, .. }
            | AuditPrincipalReference::AgentCredential { user_id, .. }
            | AuditPrincipalReference::Offline {
                user_id: Some(user_id),
            } => user_id,
            AuditPrincipalReference::Offline { user_id: None }
            | AuditPrincipalReference::Unauthenticated => {
                panic!("test agent registration requires an authorized audit principal")
            }
        };
        store
            .auth
            .register_agent_credential(auth_store::RegisterAgentCredential {
                credential_id,
                owner_user_id,
                issuer_user_id,
                public_key: NostrPublicKey::parse(public_key).unwrap(),
                label: "profile test agent".into(),
                scopes,
                created_at,
                expires_at: None,
                audit,
            })
            .await
            .unwrap();
    }

    fn profile_command(
        user_id: UserId,
        expected_version: Option<ProfileVersion>,
        address: Option<&str>,
        tips_enabled: bool,
        occurred_at: i64,
    ) -> UpdateProfile {
        UpdateProfile {
            user_id,
            precondition: ProfilePrecondition::from(expected_version),
            display_name: Some(ProfileDisplayName::parse("Alice Writer").unwrap()),
            lightning_address: address.map(|value| LightningAddress::parse(value).unwrap()),
            tips_enabled,
            occurred_at: at(occurred_at),
            audit: profile_audit(user_id, occurred_at as u128),
        }
    }

    fn recipient_command(
        actor: UserId,
        expected_version: u64,
        recipient_user_id: Option<UserId>,
        occurred_at: i64,
    ) -> SetTipRecipient {
        SetTipRecipient {
            expected_version: version(expected_version),
            recipient_user_id,
            occurred_at: at(occurred_at),
            audit: profile_audit(actor, occurred_at as u128),
        }
    }

    fn assert_command_error<T>(
        result: Result<T, ProfileMutationError>,
        expected: ProfileCommandError,
    ) {
        assert!(
            matches!(result, Err(ProfileMutationError::Command(actual)) if actual == expected),
            "unexpected mutation result"
        );
    }

    fn run_profile_crash_process(path: &Path, crash_point: &str) {
        let output = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(PROFILE_CRASH_HELPER_TEST)
            .arg("--nocapture")
            .env(PROFILE_CRASH_DATABASE_ENV, path)
            .env(WRITER_CRASH_POINT_ENV, crash_point)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "profile crash process unexpectedly survived:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("writer crash point reached"),
            "profile crash process failed before the writer crash point:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn stored_rows_fail_closed_when_their_typed_invariants_are_broken() {
        let valid_profile = || UserProfileRow {
            display_name: Some("Alice Writer".to_owned()),
            lightning_address: Some("alice@example.com".to_owned()),
            tips_enabled: 1,
            version: 1,
            updated_at_ns: 20,
        };

        let mut row = valid_profile();
        row.display_name = Some(" Alice".to_owned());
        assert!(matches!(
            StoredUserProfile::try_from_row(user(1), row),
            Err(ProfileLoadError::InvalidDisplayName)
        ));

        let mut row = valid_profile();
        row.lightning_address = Some("Alice@example.com".to_owned());
        assert!(matches!(
            StoredUserProfile::try_from_row(user(1), row),
            Err(ProfileLoadError::InvalidLightningAddress)
        ));

        let mut row = valid_profile();
        row.tips_enabled = 2;
        assert!(matches!(
            StoredUserProfile::try_from_row(user(1), row),
            Err(ProfileLoadError::InvalidBoolean)
        ));

        let mut row = valid_profile();
        row.version = 0;
        assert!(matches!(
            StoredUserProfile::try_from_row(user(1), row),
            Err(ProfileLoadError::InvalidVersion)
        ));

        assert!(matches!(
            StoredTipRecipientSetting::try_from(TipRecipientSettingRow {
                recipient_user_id: Some(vec![0; 15]),
                version: 1,
                updated_at_ns: 20,
            }),
            Err(ProfileLoadError::InvalidUserId)
        ));
        assert!(matches!(
            StoredTipRecipientSetting::try_from(TipRecipientSettingRow {
                recipient_user_id: None,
                version: -1,
                updated_at_ns: 20,
            }),
            Err(ProfileLoadError::InvalidVersion)
        ));

        let profile_receipt = || ProfileMutationReceiptRow {
            receipt_idempotency_key: Some(vec![1; 16]),
            command_fingerprint: Some(vec![2; 32]),
            result_kind: Some("user_profile".to_owned()),
            profile_user_id: Some(user(1).into_uuid().as_bytes().to_vec()),
            display_name: Some("Alice Writer".to_owned()),
            lightning_address: Some("alice@example.com".to_owned()),
            tips_enabled: Some(1),
            recipient_user_id: None,
            result_version: Some(1),
            result_updated_at_ns: Some(20),
            principal_kind: "browser_session".to_owned(),
            actor_user_id: Some(user(1).into_uuid().as_bytes().to_vec()),
            session_id: Some(vec![3; 16]),
            agent_credential_id: None,
            action: "profile.user.put".to_owned(),
            outcome: "succeeded".to_owned(),
            reason_code: None,
        };
        let mut row = profile_receipt();
        row.tips_enabled = Some(2);
        assert!(matches!(
            row.into_profile(),
            Err(ProfileApplyError::CorruptStoredState)
        ));
        let mut row = profile_receipt();
        row.recipient_user_id = Some(user(2).into_uuid().as_bytes().to_vec());
        assert!(matches!(
            row.into_profile(),
            Err(ProfileApplyError::CorruptStoredState)
        ));

        let mut row = profile_receipt();
        row.result_kind = Some("tip_recipient".to_owned());
        row.profile_user_id = None;
        row.display_name = None;
        row.lightning_address = None;
        row.tips_enabled = None;
        row.recipient_user_id = Some(vec![0; 15]);
        assert!(matches!(
            row.into_tip_recipient(),
            Err(ProfileApplyError::CorruptStoredState)
        ));
        let mut row = profile_receipt();
        row.result_version = None;
        assert!(matches!(
            row.into_profile(),
            Err(ProfileApplyError::CorruptStoredState)
        ));
    }

    #[test]
    fn projection_identity_is_stable_and_unambiguous() {
        let projection = TipRecipientProjection::from_validated_profile(
            Some(ProfileDisplayName::parse("Alice Writer").unwrap()),
            LightningAddress::parse("alice@example.com").unwrap(),
        )
        .unwrap();
        let view = projection.as_view();
        assert_eq!(view.display_name, Some("Alice Writer"));
        assert_eq!(view.address, "alice@example.com");
        assert_eq!(view.lnurl, ALICE_LNURL);
        assert_eq!(view.wallet_link, format!("lightning:{ALICE_LNURL}"));
        assert_eq!(projection, projection.clone());
        let expected_identity = [
            b"maincopy.tip-recipient.v1\0".as_slice(),
            &[1],
            &("Alice Writer".len() as u64).to_be_bytes(),
            b"Alice Writer",
            &("alice@example.com".len() as u64).to_be_bytes(),
            b"alice@example.com",
            &(ALICE_LNURL.len() as u64).to_be_bytes(),
            ALICE_LNURL.as_bytes(),
            &(view.wallet_link.len() as u64).to_be_bytes(),
            view.wallet_link.as_bytes(),
        ]
        .concat();
        assert_eq!(projection.identity_bytes(), expected_identity);

        let renamed = TipRecipientProjection::from_validated_profile(
            Some(ProfileDisplayName::parse("Alice").unwrap()),
            LightningAddress::parse("alice@example.com").unwrap(),
        )
        .unwrap();
        let moved = TipRecipientProjection::from_validated_profile(
            Some(ProfileDisplayName::parse("Alice Writer").unwrap()),
            LightningAddress::parse("alice@tips.example.com").unwrap(),
        )
        .unwrap();
        assert_ne!(renamed.identity_bytes(), projection.identity_bytes());
        assert_ne!(moved.identity_bytes(), projection.identity_bytes());
    }

    #[tokio::test]
    async fn strict_schema_rejects_profile_values_outside_storage_bounds() {
        let harness = Harness::start().await;
        let (_root, mut connection, owner) = harness.stop_and_open_writer().await;
        let owner = owner.into_uuid().into_bytes();

        for (display_name, lightning_address, tips_enabled, version) in [
            (Some("a".repeat(161)), None, 0, 1),
            (None, Some("a".repeat(321)), 0, 1),
            (None, None, 2, 1),
            (None, None, 0, 0),
        ] {
            let result = sqlx::query(
                "INSERT INTO user_profiles (\
                    user_id, display_name, lightning_address, tips_enabled, version, updated_at_ns\
                 ) VALUES (?, ?, ?, ?, ?, 20)",
            )
            .bind(owner.as_slice())
            .bind(display_name)
            .bind(lightning_address)
            .bind(tips_enabled)
            .bind(version)
            .execute(&mut connection)
            .await;
            assert!(result.is_err());
        }

        assert!(
            sqlx::query("UPDATE site_tip_recipient SET recipient_user_id = ? WHERE singleton = 1")
                .bind(vec![0_u8; 15])
                .execute(&mut connection)
                .await
                .is_err()
        );
        connection.close().await.unwrap();
    }

    async fn create_enabled_user(harness: &Harness, user_id: UserId) {
        harness
            .store
            .auth
            .create_user(auth_store::CreateUser {
                user_id,
                created_by_user_id: harness.owner,
                status: UserStatus::Enabled,
                roles: BTreeSet::from([UserRole::Administrator]),
                credentials: vec![auth_store::NewHumanCredential::Nostr {
                    public_key: NostrPublicKey::parse(OTHER_USER_KEY).unwrap(),
                }],
                configured_providers: auth_store::ConfiguredLoginProviders::new(false, true)
                    .unwrap(),
                occurred_at: at(40),
                audit: profile_audit(harness.owner, 100),
            })
            .await
            .unwrap();
        register_agent(
            &harness.store,
            user_id,
            profile_agent(user_id),
            OTHER_USER_AGENT_KEY,
            BTreeSet::from([AdminScope::ProfileManage, AdminScope::LightningManage]),
            at(41),
            profile_audit(harness.owner, 102),
        )
        .await;
    }

    async fn disable_user(harness: &Harness, user_id: UserId) {
        harness
            .store
            .auth
            .set_user_status(auth_store::SetUserStatus {
                user_id,
                changed_by_user_id: harness.owner,
                expected_version: 1,
                status: UserStatus::Disabled,
                configured_providers: auth_store::ConfiguredLoginProviders::new(false, true)
                    .unwrap(),
                occurred_at: at(42),
                audit: profile_audit(harness.owner, 101),
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn profile_updates_and_recipient_selection_use_exact_resource_versions() {
        let harness = Harness::start().await;
        let initial = harness.store.profiles.active_tip_recipient().await.unwrap();
        assert_eq!(initial.recipient_user_id, None);
        assert_eq!(initial.version, version(1));
        assert_eq!(initial.updated_at, OffsetDateTime::UNIX_EPOCH);

        let created = harness
            .store
            .profiles
            .update_profile(profile_command(
                harness.owner,
                None,
                Some("alice@example.com"),
                true,
                20,
            ))
            .await
            .unwrap();
        assert_eq!(created.version, version(1));
        assert_eq!(
            harness.store.profiles.profile(harness.owner).await.unwrap(),
            Some(created.clone())
        );
        assert_command_error(
            harness
                .store
                .profiles
                .update_profile(profile_command(
                    harness.owner,
                    None,
                    Some("other@example.com"),
                    true,
                    21,
                ))
                .await,
            ProfileCommandError::Conflict,
        );
        assert_command_error(
            harness
                .store
                .profiles
                .update_profile(profile_command(
                    harness.owner,
                    Some(version(9)),
                    Some("other@example.com"),
                    true,
                    22,
                ))
                .await,
            ProfileCommandError::StaleVersion,
        );
        assert_command_error(
            harness
                .store
                .profiles
                .update_profile(profile_command(
                    harness.owner,
                    Some(version(1)),
                    Some("other@example.com"),
                    true,
                    19,
                ))
                .await,
            ProfileCommandError::InvalidValue,
        );

        let mut clear_profile = profile_command(harness.owner, Some(version(1)), None, false, 23);
        clear_profile.display_name = None;
        let cleared_profile = harness
            .store
            .profiles
            .update_profile(clear_profile)
            .await
            .unwrap();
        assert_eq!(cleared_profile.display_name, None);
        assert_eq!(cleared_profile.lightning_address, None);
        assert!(!cleared_profile.tips_enabled);
        assert_eq!(cleared_profile.version, version(2));

        let selected = harness
            .store
            .profiles
            .set_tip_recipient(recipient_command(harness.owner, 1, Some(harness.owner), 30))
            .await
            .unwrap();
        assert_eq!(selected.version, version(2));
        assert_eq!(selected.recipient_user_id, Some(harness.owner));
        assert_command_error(
            harness
                .store
                .profiles
                .set_tip_recipient(recipient_command(harness.owner, 1, None, 31))
                .await,
            ProfileCommandError::StaleVersion,
        );
        assert_command_error(
            harness
                .store
                .profiles
                .set_tip_recipient(recipient_command(harness.owner, 2, None, 29))
                .await,
            ProfileCommandError::InvalidValue,
        );
        assert_command_error(
            harness
                .store
                .profiles
                .set_tip_recipient(recipient_command(harness.owner, 2, Some(user(999)), 32))
                .await,
            ProfileCommandError::NotFound,
        );
        let cleared = harness
            .store
            .profiles
            .set_tip_recipient(recipient_command(harness.owner, 2, None, 33))
            .await
            .unwrap();
        assert_eq!(cleared.recipient_user_id, None);
        assert_eq!(cleared.version, version(3));
        assert_eq!(
            harness.store.profiles.active_tip_recipient().await.unwrap(),
            cleared
        );
        harness.stop().await;
    }

    #[tokio::test]
    async fn profile_mutations_enforce_actor_identity_and_resource_scope() {
        let harness = Harness::start().await;

        let mut wrong_actor =
            profile_command(harness.owner, None, Some("alice@example.com"), true, 20);
        wrong_actor.audit.principal = AuditPrincipalReference::BrowserSession {
            user_id: user(999),
            session_id: AdminSessionId::from_uuid(uuid(998)),
        };
        assert_command_error(
            harness.store.profiles.update_profile(wrong_actor).await,
            ProfileCommandError::Forbidden,
        );

        let publisher = user(60);
        let create_audit = profile_audit(harness.owner, 200);
        let identity_key = create_audit.idempotency_key;
        harness
            .store
            .auth
            .create_user(auth_store::CreateUser {
                user_id: publisher,
                created_by_user_id: harness.owner,
                status: UserStatus::Enabled,
                roles: BTreeSet::from([UserRole::Publisher]),
                credentials: vec![auth_store::NewHumanCredential::Nostr {
                    public_key: NostrPublicKey::parse(OTHER_USER_KEY).unwrap(),
                }],
                configured_providers: auth_store::ConfiguredLoginProviders::new(false, true)
                    .unwrap(),
                occurred_at: at(30),
                audit: create_audit,
            })
            .await
            .unwrap();
        register_agent(
            &harness.store,
            publisher,
            profile_agent(publisher),
            OTHER_USER_AGENT_KEY,
            BTreeSet::from([AdminScope::ContentRead]),
            at(31),
            profile_audit(harness.owner, 201),
        )
        .await;
        assert_command_error(
            harness
                .store
                .profiles
                .update_profile(profile_command(
                    publisher,
                    None,
                    Some("publisher@example.com"),
                    true,
                    32,
                ))
                .await,
            ProfileCommandError::Forbidden,
        );
        assert_eq!(
            harness.store.profiles.profile(publisher).await.unwrap(),
            None
        );

        let mut cross_domain =
            profile_command(harness.owner, None, Some("alice@example.com"), true, 33);
        cross_domain.audit.idempotency_key = identity_key;
        assert_command_error(
            harness.store.profiles.update_profile(cross_domain).await,
            ProfileCommandError::IdempotencyConflict,
        );
        assert_eq!(
            harness.store.profiles.profile(harness.owner).await.unwrap(),
            None
        );

        let mut offline = recipient_command(harness.owner, 1, None, 34);
        offline.audit.principal = AuditPrincipalReference::Offline {
            user_id: Some(harness.owner),
        };
        assert_command_error(
            harness.store.profiles.set_tip_recipient(offline).await,
            ProfileCommandError::Forbidden,
        );
        harness.stop().await;
    }

    #[tokio::test]
    async fn effective_recipient_requires_enabled_user_profile_address_and_opt_in() {
        let harness = Harness::start().await;
        assert_eq!(
            harness
                .store
                .profiles
                .effective_tip_recipient()
                .await
                .unwrap(),
            None
        );
        harness
            .store
            .profiles
            .set_tip_recipient(recipient_command(harness.owner, 1, Some(harness.owner), 19))
            .await
            .unwrap();
        assert_eq!(
            harness
                .store
                .profiles
                .effective_tip_recipient()
                .await
                .unwrap(),
            None
        );
        harness
            .store
            .profiles
            .update_profile(profile_command(harness.owner, None, None, true, 20))
            .await
            .unwrap();
        assert_eq!(
            harness
                .store
                .profiles
                .effective_tip_recipient()
                .await
                .unwrap(),
            None
        );

        harness
            .store
            .profiles
            .update_profile(profile_command(
                harness.owner,
                Some(version(1)),
                Some("alice@example.com"),
                false,
                21,
            ))
            .await
            .unwrap();
        assert_eq!(
            harness
                .store
                .profiles
                .effective_tip_recipient()
                .await
                .unwrap(),
            None
        );
        harness
            .store
            .profiles
            .update_profile(profile_command(
                harness.owner,
                Some(version(2)),
                Some("alice@example.com"),
                true,
                22,
            ))
            .await
            .unwrap();
        let projection = harness
            .store
            .profiles
            .effective_tip_recipient()
            .await
            .unwrap()
            .unwrap();
        let view = projection.as_view();
        assert_eq!(view.display_name, Some("Alice Writer"));
        assert_eq!(view.address, "alice@example.com");
        assert_eq!(view.lnurl, ALICE_LNURL);
        assert!(view.lnurl.bytes().all(|byte| !byte.is_ascii_lowercase()));
        let decoded = CheckedHrpstring::new::<Bech32>(view.lnurl).unwrap();
        assert_eq!(decoded.hrp(), LNURL_HRP);
        assert_eq!(
            decoded.byte_iter().collect::<Vec<_>>(),
            ALICE_ENDPOINT.as_bytes()
        );
        assert_eq!(view.wallet_link, format!("lightning:{}", view.lnurl));

        let disabled_user = user(50);
        create_enabled_user(&harness, disabled_user).await;
        harness
            .store
            .profiles
            .update_profile(profile_command(
                disabled_user,
                None,
                Some("disabled@example.com"),
                true,
                41,
            ))
            .await
            .unwrap();
        disable_user(&harness, disabled_user).await;
        harness
            .store
            .profiles
            .set_tip_recipient(recipient_command(harness.owner, 2, Some(disabled_user), 45))
            .await
            .unwrap();
        assert_eq!(
            harness
                .store
                .profiles
                .effective_tip_recipient()
                .await
                .unwrap(),
            None
        );
        harness.stop().await;
    }

    #[tokio::test]
    async fn mutation_keys_replay_exact_results_and_bind_action_body_and_principal() {
        let harness = Harness::start().await;
        let original = profile_command(harness.owner, None, Some("alice@example.com"), true, 20);
        let original_result = harness
            .store
            .profiles
            .update_profile(original.clone())
            .await
            .unwrap();
        let advanced_result = harness
            .store
            .profiles
            .update_profile(profile_command(
                harness.owner,
                Some(version(1)),
                Some("alice@tips.example.com"),
                true,
                21,
            ))
            .await
            .unwrap();
        assert_eq!(advanced_result.version, version(2));

        let mut retry = original.clone();
        retry.occurred_at = at(99);
        retry.audit.audit_event_id = AdminAuditEventId::from_uuid(uuid(99_001));
        retry.audit.request_id = Some(uuid(99_002));
        assert_eq!(
            harness.store.profiles.update_profile(retry).await.unwrap(),
            original_result
        );
        assert_eq!(
            harness.store.profiles.profile(harness.owner).await.unwrap(),
            Some(advanced_result)
        );

        let mut changed_body = original.clone();
        changed_body.lightning_address =
            Some(LightningAddress::parse("other@example.com").unwrap());
        assert_command_error(
            harness.store.profiles.update_profile(changed_body).await,
            ProfileCommandError::IdempotencyConflict,
        );
        let mut changed_principal = original.clone();
        changed_principal.audit.principal = AuditPrincipalReference::AgentCredential {
            user_id: harness.owner,
            credential_id: second_profile_agent(harness.owner),
        };
        assert_command_error(
            harness
                .store
                .profiles
                .update_profile(changed_principal)
                .await,
            ProfileCommandError::IdempotencyConflict,
        );

        let selected = recipient_command(harness.owner, 1, Some(harness.owner), 30);
        let selected_result = harness
            .store
            .profiles
            .set_tip_recipient(selected.clone())
            .await
            .unwrap();
        let cleared_result = harness
            .store
            .profiles
            .set_tip_recipient(recipient_command(harness.owner, 2, None, 31))
            .await
            .unwrap();
        assert_eq!(cleared_result.version, version(3));
        assert_eq!(
            harness
                .store
                .profiles
                .set_tip_recipient(selected.clone())
                .await
                .unwrap(),
            selected_result
        );

        let mut changed_action = recipient_command(harness.owner, 3, None, 32);
        changed_action.audit.idempotency_key = original.audit.idempotency_key;
        assert_command_error(
            harness
                .store
                .profiles
                .set_tip_recipient(changed_action)
                .await,
            ProfileCommandError::IdempotencyConflict,
        );

        let (_root, mut connection, _) = harness.stop_and_open_writer().await;
        let audit_rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT action, outcome, reason_code FROM admin_audit_events \
             WHERE action LIKE 'profile.%' ORDER BY occurred_at_ns, audit_event_id",
        )
        .fetch_all(&mut connection)
        .await
        .unwrap();
        assert_eq!(
            audit_rows,
            [
                ("profile.user.put".to_owned(), "succeeded".to_owned(), None),
                ("profile.user.put".to_owned(), "succeeded".to_owned(), None),
                (
                    "profile.tip-recipient.replace".to_owned(),
                    "succeeded".to_owned(),
                    None,
                ),
                (
                    "profile.tip-recipient.replace".to_owned(),
                    "succeeded".to_owned(),
                    None,
                ),
            ]
        );
        let receipt_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM admin_profile_mutation_receipts")
                .fetch_one(&mut connection)
                .await
                .unwrap();
        assert_eq!(receipt_count, 4);
        connection.close().await.unwrap();
    }

    #[tokio::test]
    async fn profile_update_crash_process() {
        let Some(path) = std::env::var_os(PROFILE_CRASH_DATABASE_ENV) else {
            return;
        };
        let database = database::bootstrap(configuration(&PathBuf::from(path)))
            .await
            .unwrap();
        let (store, writer) = database.into_store(1);
        let _writer = tokio::spawn(writer.run(CancellationToken::new()));
        let response = store
            .profiles
            .update_profile(profile_command(
                user(2),
                None,
                Some("alice@example.com"),
                true,
                20,
            ))
            .await;
        panic!("writer did not abort: response={response:?}");
    }

    #[tokio::test]
    async fn committed_receipt_makes_profile_retry_safe_across_writer_crashes() {
        for (crash_point, committed_rows) in [
            ("after-apply-before-commit", 0_i64),
            ("after-commit-before-reply", 1_i64),
        ] {
            let harness = Harness::start().await;
            let (root, connection, owner) = harness.stop_and_open_writer().await;
            connection.close().await.unwrap();
            let path = root.path().join("state/maincopy.db");

            run_profile_crash_process(&path, crash_point);

            let reopened = database::bootstrap(configuration(&path)).await.unwrap();
            reopened.close().await.unwrap();
            let mut connection = SqliteConnection::connect(path.to_str().unwrap())
                .await
                .unwrap();
            let profile_count: i64 = sqlx::query_scalar("SELECT count(*) FROM user_profiles")
                .fetch_one(&mut connection)
                .await
                .unwrap();
            let receipt_count: i64 =
                sqlx::query_scalar("SELECT count(*) FROM admin_profile_mutation_receipts")
                    .fetch_one(&mut connection)
                    .await
                    .unwrap();
            assert_eq!(profile_count, committed_rows);
            assert_eq!(receipt_count, committed_rows);
            connection.close().await.unwrap();

            let reopened = database::bootstrap(configuration(&path)).await.unwrap();
            let (store, writer) = reopened.into_store(1);
            let shutdown = CancellationToken::new();
            let task_shutdown = shutdown.clone();
            let writer = tokio::spawn(async move { writer.run(task_shutdown).await.unwrap() });
            let result = store
                .profiles
                .update_profile(profile_command(
                    owner,
                    None,
                    Some("alice@example.com"),
                    true,
                    20,
                ))
                .await
                .unwrap();
            assert_eq!(result.version, version(1));
            drop(store);
            shutdown.cancel();
            writer.await.unwrap();

            let mut connection = SqliteConnection::connect(path.to_str().unwrap())
                .await
                .unwrap();
            let counts: (i64, i64, i64) = sqlx::query_as(
                "SELECT \
                    (SELECT count(*) FROM user_profiles), \
                    (SELECT count(*) FROM admin_profile_mutation_receipts), \
                    (SELECT count(*) FROM admin_audit_events WHERE action = 'profile.user.put')",
            )
            .fetch_one(&mut connection)
            .await
            .unwrap();
            assert_eq!(counts, (1, 1, 1));
            connection.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn committed_profile_and_projection_rehydrate_after_writer_restart() {
        let harness = Harness::start().await;
        let stored = harness
            .store
            .profiles
            .update_profile(profile_command(
                harness.owner,
                None,
                Some("alice@example.com"),
                true,
                20,
            ))
            .await
            .unwrap();
        let setting = harness
            .store
            .profiles
            .set_tip_recipient(recipient_command(harness.owner, 1, Some(harness.owner), 21))
            .await
            .unwrap();

        let harness = harness.restart().await;
        assert_eq!(
            harness.store.profiles.profile(harness.owner).await.unwrap(),
            Some(stored)
        );
        assert_eq!(
            harness.store.profiles.active_tip_recipient().await.unwrap(),
            setting
        );
        assert_eq!(
            harness
                .store
                .profiles
                .effective_tip_recipient()
                .await
                .unwrap()
                .unwrap()
                .as_view()
                .lnurl,
            ALICE_LNURL
        );
        harness.stop().await;
    }
}
