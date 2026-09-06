use maincopy_shared::auth_api::{
    ADMIN_USER_PATH, ADMIN_USER_ROLES_PATH, ADMIN_USER_STATUS_PATH, ADMIN_USERS_PATH,
    ListUsersResponse, MAX_IDENTITY_PAGE_LIMIT, ReplaceUserRolesRequest, SetUserStatusRequest,
    UserMutationResponse, UserResponse,
};
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use uuid::Uuid;

use super::{AdminClient, AdminClientError, AdminOrigin, decode_status_json};

impl AdminClient {
    pub(crate) async fn set_user_status(
        &self,
        user_id: Uuid,
        operation: Uuid,
        request: &SetUserStatusRequest,
    ) -> Result<UserMutationResponse, AdminClientError> {
        self.change_user(
            ADMIN_USER_STATUS_PATH,
            user_id,
            operation,
            request.expected_version,
            request,
        )
        .await
    }

    pub(crate) async fn replace_user_roles(
        &self,
        user_id: Uuid,
        operation: Uuid,
        request: &ReplaceUserRolesRequest,
    ) -> Result<UserMutationResponse, AdminClientError> {
        self.change_user(
            ADMIN_USER_ROLES_PATH,
            user_id,
            operation,
            request.expected_version,
            request,
        )
        .await
    }

    async fn change_user(
        &self,
        route: &str,
        user_id: Uuid,
        operation: Uuid,
        expected_version: u64,
        request: &impl Serialize,
    ) -> Result<UserMutationResponse, AdminClientError> {
        let path = route.replace("{user_id}", &user_id.to_string());
        let response = self
            .json_mutation(Method::PUT, &path, request, operation)
            .await?;
        let receipt = decode_status_json(response, StatusCode::OK)?;
        validate_receipt(receipt, user_id, expected_version)
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
    if user.user_id.into_uuid() != expected || user.version == 0 {
        return Err(invalid_response());
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
