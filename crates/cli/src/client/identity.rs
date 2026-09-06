use crate::nip98::inspect_public_key;
use maincopy_shared::auth::HumanLoginProvider;
use maincopy_shared::auth_api::{
    ADMIN_USER_CREDENTIAL_PATH, ADMIN_USER_PATH, ADMIN_USER_ROLES_PATH, ADMIN_USER_STATUS_PATH,
    ADMIN_USERS_PATH, CreateUserRequest, ExpectedVersionRequest, HumanCredentialResponse,
    ListUsersResponse, MAX_IDENTITY_PAGE_LIMIT, PutHumanCredentialRequest, ReplaceUserRolesRequest,
    SetUserStatusRequest, UserMutationResponse, UserResponse,
};
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use uuid::Uuid;

use super::{AdminClient, AdminClientError, AdminOrigin, decode_status_json};
use crate::transport::{HttpResponse, RequestBody};

pub(crate) enum UserMutation {
    Create(CreateUserRequest),
    Status {
        user_id: Uuid,
        request: SetUserStatusRequest,
    },
    Roles {
        user_id: Uuid,
        request: ReplaceUserRolesRequest,
    },
    PutCredential {
        user_id: Uuid,
        request: PutHumanCredentialRequest,
    },
    RemoveCredential {
        user_id: Uuid,
        provider: HumanLoginProvider,
        request: ExpectedVersionRequest,
    },
}

struct PreparedUserMutation {
    method: Method,
    path: String,
    body: RequestBody,
    receipt: UserReceiptExpectation,
}

enum UserReceiptExpectation {
    Created,
    AccountVersion {
        user_id: Uuid,
        expected_version: u64,
    },
    Credential {
        user_id: Uuid,
    },
}

impl PreparedUserMutation {
    fn new(
        method: Method,
        path: String,
        value: &impl Serialize,
        receipt: UserReceiptExpectation,
    ) -> Result<Self, AdminClientError> {
        Ok(Self {
            method,
            path,
            body: RequestBody::json(value).map_err(AdminClientError::RequestEncoding)?,
            receipt,
        })
    }
}

impl UserMutation {
    fn prepare(self) -> Result<PreparedUserMutation, AdminClientError> {
        match self {
            Self::Create(request) => PreparedUserMutation::new(
                Method::POST,
                ADMIN_USERS_PATH.to_string(),
                &request,
                UserReceiptExpectation::Created,
            ),
            Self::Status { user_id, request } => PreparedUserMutation::new(
                Method::PUT,
                ADMIN_USER_STATUS_PATH.replace("{user_id}", &user_id.to_string()),
                &request,
                UserReceiptExpectation::AccountVersion {
                    user_id,
                    expected_version: request.expected_version,
                },
            ),
            Self::Roles { user_id, request } => PreparedUserMutation::new(
                Method::PUT,
                ADMIN_USER_ROLES_PATH.replace("{user_id}", &user_id.to_string()),
                &request,
                UserReceiptExpectation::AccountVersion {
                    user_id,
                    expected_version: request.expected_version,
                },
            ),
            Self::PutCredential { user_id, request } => {
                let provider = match &request {
                    PutHumanCredentialRequest::Create { credential }
                    | PutHumanCredentialRequest::Replace { credential, .. } => {
                        credential.provider()
                    }
                };
                PreparedUserMutation::new(
                    Method::PUT,
                    credential_path(user_id, provider),
                    &request,
                    UserReceiptExpectation::Credential { user_id },
                )
            }
            Self::RemoveCredential {
                user_id,
                provider,
                request,
            } => PreparedUserMutation::new(
                Method::DELETE,
                credential_path(user_id, provider),
                &request,
                UserReceiptExpectation::Credential { user_id },
            ),
        }
    }
}

fn credential_path(user_id: Uuid, provider: HumanLoginProvider) -> String {
    ADMIN_USER_CREDENTIAL_PATH
        .replace("{user_id}", &user_id.to_string())
        .replace("{provider}", provider.as_str())
}

impl UserReceiptExpectation {
    fn decode(self, response: HttpResponse) -> Result<UserMutationResponse, AdminClientError> {
        match self {
            Self::Created => {
                let receipt: UserMutationResponse =
                    decode_status_json(response, StatusCode::CREATED)?;
                if receipt.version != 1 || receipt.user_id.into_uuid().is_nil() {
                    return Err(invalid_response());
                }
                Ok(receipt)
            }
            Self::AccountVersion {
                user_id,
                expected_version,
            } => validate_receipt(
                decode_status_json(response, StatusCode::OK)?,
                user_id,
                expected_version,
            ),
            Self::Credential { user_id } => {
                validate_credential_receipt(decode_status_json(response, StatusCode::OK)?, user_id)
            }
        }
    }
}

impl AdminClient {
    pub(crate) async fn change_account(
        &self,
        operation: Uuid,
        change: UserMutation,
    ) -> Result<UserMutationResponse, AdminClientError> {
        let prepared = change.prepare()?;
        let response = self
            .authenticated_request(
                prepared.method,
                &prepared.path,
                prepared.body,
                Some(operation),
            )
            .await?;
        prepared.receipt.decode(response)
    }

    pub(crate) async fn list_users(
        &self,
        cursor: Option<Uuid>,
    ) -> Result<ListUsersResponse, AdminClientError> {
        let page = self.get_json(users_url(&self.origin, cursor)?).await?;
        validate_page(page, cursor)
    }

    pub(crate) async fn inspect_user(
        &self,
        user_id: Uuid,
    ) -> Result<UserResponse, AdminClientError> {
        let path = ADMIN_USER_PATH.replace("{user_id}", &user_id.to_string());
        let user: UserResponse = self.get_json(self.origin.request_url(&path)?).await?;
        validate_user(user, user_id)
    }
}

fn users_url(origin: &AdminOrigin, cursor: Option<Uuid>) -> Result<Url, AdminClientError> {
    let mut url = origin.request_url(ADMIN_USERS_PATH)?;
    url.query_pairs_mut()
        .append_pair("limit", &MAX_IDENTITY_PAGE_LIMIT.to_string());
    if let Some(cursor) = cursor {
        url.query_pairs_mut()
            .append_pair("cursor", &cursor.to_string());
    }
    Ok(url)
}

fn validate_receipt(
    receipt: UserMutationResponse,
    expected_user: Uuid,
    expected_version: u64,
) -> Result<UserMutationResponse, AdminClientError> {
    if receipt.user_id.into_uuid() != expected_user
        || expected_version.checked_add(1) != Some(receipt.version)
    {
        return Err(invalid_response());
    }
    Ok(receipt)
}

// The precondition is a credential version; the receipt contains the aggregate user version.
fn validate_credential_receipt(
    receipt: UserMutationResponse,
    user_id: Uuid,
) -> Result<UserMutationResponse, AdminClientError> {
    if receipt.user_id.into_uuid() != user_id || receipt.version == 0 {
        return Err(invalid_response());
    }
    Ok(receipt)
}

fn invalid_response() -> AdminClientError {
    AdminClientError::InvalidIdentityResponse {
        message: "account identifiers, versions, or pagination are inconsistent",
    }
}

fn validate_page(
    page: ListUsersResponse,
    cursor: Option<Uuid>,
) -> Result<ListUsersResponse, AdminClientError> {
    if page.users.len() > usize::from(MAX_IDENTITY_PAGE_LIMIT) {
        return Err(invalid_response());
    }
    let mut previous = cursor;
    for user in &page.users {
        let current = user.user_id.into_uuid();
        if user.version == 0 || previous.is_some_and(|value| value >= current) {
            return Err(invalid_response());
        }
        previous = Some(current);
    }
    if let Some(next) = page.next_cursor
        && page.users.last().map(|user| user.user_id) != Some(next)
    {
        return Err(invalid_response());
    }
    Ok(page)
}

fn validate_user(user: UserResponse, expected: Uuid) -> Result<UserResponse, AdminClientError> {
    if user.user_id.into_uuid() != expected || user.version == 0 || user.credentials.len() > 2 {
        return Err(invalid_response());
    }
    let mut providers = Vec::new();
    for credential in &user.credentials {
        let (provider, version) = match credential {
            HumanCredentialResponse::Password { version, .. } => {
                (HumanLoginProvider::Password, *version)
            }
            HumanCredentialResponse::Nostr {
                public_key,
                version,
                ..
            } => {
                inspect_public_key(public_key).map_err(|_| invalid_response())?;
                (HumanLoginProvider::Nostr, *version)
            }
        };
        if version == 0 || providers.contains(&provider) {
            return Err(invalid_response());
        }
        providers.push(provider);
    }
    Ok(user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maincopy_shared::{auth::UserId, auth_api::UserSummaryResponse};
    use serde_json::json;

    fn summary(id: u128) -> UserSummaryResponse {
        serde_json::from_value(json!({"user_id": Uuid::from_u128(id), "status":"enabled", "version":1, "roles":["publisher"], "scopes":["content_read"], "credential_providers":["password"], "created_at":"2026-09-06T12:00:00Z", "updated_at":"2026-09-06T12:00:00Z"})).unwrap()
    }

    fn response(status: StatusCode, user_id: Uuid, version: u64) -> HttpResponse {
        HttpResponse {
            status,
            headers: reqwest::header::HeaderMap::from_iter([(
                reqwest::header::CONTENT_TYPE,
                reqwest::header::HeaderValue::from_static("application/json"),
            )]),
            body: serde_json::to_vec(&UserMutationResponse {
                user_id: user_id.into(),
                version,
            })
            .unwrap(),
        }
    }

    #[test]
    fn account_mutations_prepare_exact_methods_routes_and_receipt_expectations() {
        use maincopy_shared::{
            auth::{UserRole, UserStatus},
            auth_api::{HumanCredentialInput, SecretString},
        };
        let user_id = Uuid::from_u128(1);
        let mutations = [
            (
                UserMutation::Create(CreateUserRequest {
                    status: UserStatus::Enabled,
                    roles: vec![UserRole::Publisher],
                    credentials: vec![HumanCredentialInput::Nostr {
                        public_key:
                            "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9"
                                .into(),
                    }],
                }),
                Method::POST,
                ADMIN_USERS_PATH.to_owned(),
                StatusCode::CREATED,
                1,
            ),
            (
                UserMutation::Status {
                    user_id,
                    request: SetUserStatusRequest {
                        expected_version: 4,
                        status: UserStatus::Disabled,
                    },
                },
                Method::PUT,
                format!("{ADMIN_USERS_PATH}/{user_id}/status"),
                StatusCode::OK,
                5,
            ),
            (
                UserMutation::Roles {
                    user_id,
                    request: ReplaceUserRolesRequest {
                        expected_version: 4,
                        roles: vec![UserRole::Publisher],
                    },
                },
                Method::PUT,
                format!("{ADMIN_USERS_PATH}/{user_id}/roles"),
                StatusCode::OK,
                5,
            ),
            (
                UserMutation::PutCredential {
                    user_id,
                    request: PutHumanCredentialRequest::Create {
                        credential: HumanCredentialInput::Password {
                            username: "fixture".into(),
                            password: SecretString::new("long fixture password"),
                        },
                    },
                },
                Method::PUT,
                format!("{ADMIN_USERS_PATH}/{user_id}/credentials/password"),
                StatusCode::OK,
                9,
            ),
            (
                UserMutation::PutCredential {
                    user_id,
                    request: PutHumanCredentialRequest::Replace {
                        expected_version: 2,
                        credential: HumanCredentialInput::Nostr {
                            public_key:
                                "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9"
                                    .into(),
                        },
                    },
                },
                Method::PUT,
                format!("{ADMIN_USERS_PATH}/{user_id}/credentials/nostr"),
                StatusCode::OK,
                9,
            ),
            (
                UserMutation::RemoveCredential {
                    user_id,
                    provider: HumanLoginProvider::Password,
                    request: ExpectedVersionRequest {
                        expected_version: 2,
                    },
                },
                Method::DELETE,
                format!("{ADMIN_USERS_PATH}/{user_id}/credentials/password"),
                StatusCode::OK,
                9,
            ),
        ];
        for (mutation, method, path, status, version) in mutations {
            let prepared = mutation.prepare().unwrap();
            assert_eq!(prepared.method, method);
            assert_eq!(prepared.path, path);
            let body: serde_json::Value = serde_json::from_slice(prepared.body.as_ref()).unwrap();
            assert!(body.is_object());
            let receipt = prepared
                .receipt
                .decode(response(status, user_id, version))
                .unwrap();
            assert_eq!(receipt.user_id.into_uuid(), user_id);
            assert_eq!(receipt.version, version);
        }
    }

    #[test]
    fn creation_receipts_require_a_created_nonempty_account_at_version_one() {
        let user_id = Uuid::from_u128(1);
        for (status, id, version) in [
            (StatusCode::OK, user_id, 1),
            (StatusCode::CREATED, Uuid::nil(), 1),
            (StatusCode::CREATED, user_id, 2),
        ] {
            assert!(
                UserReceiptExpectation::Created
                    .decode(response(status, id, version))
                    .is_err()
            );
        }
    }

    #[test]
    fn account_inspection_rejects_invalid_duplicate_or_unversioned_credentials() {
        let id = Uuid::from_u128(1);
        let mut value = serde_json::to_value(summary(1)).unwrap();
        value["credentials"] = json!([
            {"provider":"password", "username":"fixture", "version":2, "created_at":"2026-09-06T12:00:00Z", "updated_at":"2026-09-06T12:00:00Z"},
            {"provider":"nostr", "public_key":"f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9", "version":3, "created_at":"2026-09-06T12:00:00Z", "updated_at":"2026-09-06T12:00:00Z"}
        ]);
        let user: UserResponse = serde_json::from_value(value.clone()).unwrap();
        assert!(validate_user(user, id).is_ok());
        for invalid in 0..6 {
            let mut changed = value.clone();
            match invalid {
                0 => changed["credentials"][0]["version"] = json!(0),
                1 => changed["credentials"][1]["version"] = json!(0),
                2 => changed["credentials"][1]["public_key"] = json!("0".repeat(64)),
                3 => changed["credentials"][0] = value["credentials"][1].clone(),
                4 => changed["credentials"][1] = value["credentials"][0].clone(),
                _ => changed["credentials"]
                    .as_array_mut()
                    .unwrap()
                    .push(value["credentials"][0].clone()),
            }
            assert!(validate_user(serde_json::from_value(changed).unwrap(), id).is_err());
        }
    }

    #[test]
    fn user_page_targets_keep_the_origin_port_and_bound_each_request() {
        let origin = AdminOrigin::parse("https://admin.example.test:8443").unwrap();
        for cursor in [None, Some(Uuid::from_u128(1))] {
            let url = users_url(&origin, cursor).unwrap();
            assert_eq!(url.origin().ascii_serialization(), origin.as_str());
            assert_eq!(url.path(), ADMIN_USERS_PATH);
            let mut expected = vec![("limit".to_owned(), "100".to_owned())];
            if let Some(cursor) = cursor {
                expected.push(("cursor".to_owned(), cursor.to_string()));
            }
            assert_eq!(url.query_pairs().into_owned().collect::<Vec<_>>(), expected);
        }
    }

    #[test]
    fn account_pages_reject_duplicate_backwards_oversized_and_unrecoverable_results() {
        let page = ListUsersResponse {
            users: vec![summary(2), summary(3)],
            next_cursor: Some(UserId::from_uuid(Uuid::from_u128(3))),
        };
        assert_eq!(
            validate_page(page.clone(), Some(Uuid::from_u128(1))).unwrap(),
            page
        );
        let empty = ListUsersResponse {
            users: Vec::new(),
            next_cursor: None,
        };
        assert!(validate_page(empty.clone(), None).is_ok());
        let mut invalid = Vec::new();
        let mut duplicate = page.clone();
        duplicate.users[1] = summary(2);
        invalid.push(duplicate);
        let mut backwards = page.clone();
        backwards.users.reverse();
        invalid.push(backwards);
        let mut oversized = page.clone();
        oversized.users = (1..=101).map(summary).collect();
        invalid.push(oversized);
        let mut cursor = page.clone();
        cursor.next_cursor = Some(UserId::from_uuid(Uuid::from_u128(4)));
        invalid.push(cursor);
        let mut version = page.clone();
        version.users[0].version = 0;
        invalid.push(version);
        let mut empty_cursor = empty;
        empty_cursor.next_cursor = page.next_cursor;
        invalid.push(empty_cursor);
        for page in invalid {
            assert!(matches!(
                validate_page(page.clone(), None),
                Err(AdminClientError::InvalidIdentityResponse { .. })
            ));
        }
        assert!(validate_page(page.clone(), Some(Uuid::from_u128(2))).is_err());
    }

    #[test]
    fn account_inspection_rejects_a_different_account_or_missing_version() {
        let id = Uuid::from_u128(1);
        let mut value = serde_json::to_value(summary(1)).unwrap();
        value["credentials"] = json!([]);
        let mut user: UserResponse = serde_json::from_value(value).unwrap();
        assert_eq!(validate_user(user.clone(), id).unwrap(), user);
        assert!(validate_user(user.clone(), Uuid::from_u128(2)).is_err());
        user.version = 0;
        assert!(validate_user(user.clone(), id).is_err());
    }
    #[test]
    fn credential_receipts_keep_aggregate_user_versions_separate_from_credential_preconditions() {
        let user_id = Uuid::from_u128(1);
        let receipt = UserMutationResponse {
            user_id: user_id.into(),
            version: 9,
        };
        assert_eq!(
            validate_credential_receipt(receipt, user_id).unwrap(),
            receipt
        );
        assert!(validate_credential_receipt(receipt, Uuid::from_u128(2)).is_err());
        assert!(
            validate_credential_receipt(
                UserMutationResponse {
                    version: 0,
                    ..receipt
                },
                user_id
            )
            .is_err()
        );
    }

    #[test]
    fn account_receipts_bind_the_target_and_exact_next_version() {
        let user_id = Uuid::from_u128(1);
        let receipt = UserMutationResponse {
            user_id: user_id.into(),
            version: 5,
        };
        assert_eq!(validate_receipt(receipt, user_id, 4).unwrap(), receipt);
        assert!(validate_receipt(receipt, Uuid::from_u128(2), 4).is_err());
        assert!(validate_receipt(receipt, user_id, 3).is_err());
        assert!(validate_receipt(receipt, user_id, u64::MAX).is_err());
    }
}
