use axum::{
    Extension, Form, Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{FormRejection, PathRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header::RETRY_AFTER},
    middleware,
    response::Response,
    routing::{get, post},
};
use maincopy_shared::{
    auth::{AdminScope, HumanLoginProvider, UserId, UserRole, UserStatus},
    auth_api::{
        CreateUserRequest, ExpectedVersionRequest, HumanCredentialInput, HumanCredentialResponse,
        PutHumanCredentialRequest, ReplaceUserRolesRequest, SecretString, SetUserStatusRequest,
        UserResponse,
    },
    publication::IDEMPOTENCY_KEY_HEADER,
};
use maud::{Markup, html};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    PageQuery, create_user_command, identity_page, load_problem, not_found, put_human_credential,
    remove_human_credential, replace_user_roles, set_user_status, user_path, user_response,
};
use crate::{
    admin::{
        AdminRuntimeState, AdminSecurityState, BrowserFormSession, BrowserSessionContext,
        RequiredBrowserSession, browser_scoped_router,
        principal::AdminPrincipal,
        request_id::RequestId,
        ui::{
            PageKind, adapt_security_response, error_response, mutation_error_response,
            page_response, redirect,
        },
    },
    domain::{
        auth::{MAX_PASSWORD_SCALARS, MAX_USERNAME_BYTES, MIN_PASSWORD_SCALARS},
        publication::activation::PublicationCoordinatorHandle,
    },
};

const FORM_LIMIT: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
enum CreateUserForm {
    Password {
        _csrf: SecretString,
        operation_id: Uuid,
        role: UserRole,
        username: Box<str>,
        password: SecretString,
        confirmation: SecretString,
    },
    Nostr {
        _csrf: SecretString,
        operation_id: Uuid,
        role: UserRole,
        public_key: Box<str>,
    },
}

impl CreateUserForm {
    fn into_request(self) -> Result<(Uuid, CreateUserRequest), PasswordConfirmationError> {
        let (operation_id, role, credential) = match self {
            Self::Password {
                operation_id,
                role,
                username,
                password,
                confirmation,
                ..
            } => (
                operation_id,
                role,
                password_credential(username, password, confirmation)?,
            ),
            Self::Nostr {
                operation_id,
                role,
                public_key,
                ..
            } => (
                operation_id,
                role,
                HumanCredentialInput::Nostr { public_key },
            ),
        };
        Ok((
            operation_id,
            CreateUserRequest {
                status: UserStatus::Enabled,
                roles: vec![role],
                credentials: vec![credential],
            },
        ))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("the password and confirmation do not match")]
struct PasswordConfirmationError;

fn password_credential(
    username: Box<str>,
    password: SecretString,
    confirmation: SecretString,
) -> Result<HumanCredentialInput, PasswordConfirmationError> {
    if password.expose_secret() != confirmation.expose_secret() {
        return Err(PasswordConfirmationError);
    }
    Ok(HumanCredentialInput::Password { username, password })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordForm {
    _csrf: SecretString,
    operation_id: Uuid,
    expected_version: Option<u64>,
    username: Box<str>,
    password: SecretString,
    confirmation: SecretString,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NostrForm {
    _csrf: SecretString,
    operation_id: Uuid,
    expected_version: Option<u64>,
    public_key: Box<str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserStatusForm {
    _csrf: SecretString,
    operation_id: Uuid,
    expected_version: u64,
    status: UserStatus,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleForm {
    _csrf: SecretString,
    operation_id: Uuid,
    expected_version: u64,
    role: UserRole,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveCredentialForm {
    _csrf: SecretString,
    operation_id: Uuid,
    expected_version: u64,
    confirm: bool,
}

pub(in crate::admin) fn browser_router(security: &AdminSecurityState) -> Router<AdminRuntimeState> {
    browser_scoped_router(
        Router::new()
            .route("/admin/users", get(show_users).post(save_new_user))
            .route("/admin/users/{user_id}", get(show_user))
            .route("/admin/users/{user_id}/status", post(save_status)),
        security,
        AdminScope::UserManage,
    )
    .merge(browser_scoped_router(
        Router::new().route("/admin/users/{user_id}/roles", post(save_role)),
        security,
        AdminScope::RoleAssign,
    ))
    .merge(browser_scoped_router(
        Router::new()
            .route("/admin/users/{user_id}/password", post(save_password))
            .route("/admin/users/{user_id}/nostr", post(save_nostr))
            .route(
                "/admin/users/{user_id}/credentials/{provider}/remove",
                post(remove_credential),
            ),
        security,
        AdminScope::CredentialManage,
    ))
    .layer(DefaultBodyLimit::max(FORM_LIMIT))
    .layer(middleware::from_fn(adapt_security_response))
}

async fn show_users(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    principal: AdminPrincipal,
    browser: BrowserFormSession,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let (cursor, limit) = match identity_page(query, request_id) {
        Ok(page) => page,
        Err(response) => return mutation_response(response, "/admin/users", request_id),
    };
    let page = match security
        .store
        .users_page(cursor.map(UserId::from_uuid), limit)
        .await
    {
        Ok(page) => page,
        Err(error) => {
            return mutation_response(load_problem(error, request_id), "/admin/users", request_id);
        }
    };
    let mut users = Vec::with_capacity(page.items.len());
    for user in page.items {
        let credentials = match security.store.user_credentials(user.user_id).await {
            Ok(Some(credentials)) => credentials,
            Ok(None) => {
                return mutation_response(not_found(request_id), "/admin/users", request_id);
            }
            Err(error) => {
                return mutation_response(
                    load_problem(error, request_id),
                    "/admin/users",
                    request_id,
                );
            }
        };
        users.push(user_response(&user, credentials));
    }
    let fresh = OffsetDateTime::now_utc() < browser.session.fresh_until;
    page_response(
        StatusCode::OK,
        "Users",
        PageKind::Authenticated,
        html! {
            section class="panel" {
                h1 { "Users" }
                p { "Signed in as " code { (browser.session.user_id) } }
                p { a href=(format!("/admin/users/{}", browser.session.user_id)) { "Manage your account" } }
                @if users.is_empty() { p { "No users on this page." } }
                @for user in &users {
                    article class="panel" {
                        h2 { a href=(format!("/admin/users/{}", user.user_id)) { (user_label(user)) } }
                        p { code { (user.user_id) } " · " (user.status.as_str()) " · version " (user.version) }
                        p { "Roles: " @for role in &user.roles { code { (role.as_str()) } " " } }
                    }
                }
                @if let Some(cursor) = page.next_cursor { a class="button" href=(format!("/admin/users?cursor={cursor}")) { "Next page" } }
            }
            section class="panel" {
                h2 { "Create user" }
                @if !fresh {
                    p class="notice" { "Creating an account requires a recent sign-in. " a href="/admin/login" { "Sign in again" } }
                }
                fieldset disabled[!fresh] {
                    legend { "Initial login credential" }
                    (create_user_forms(&security, &principal, &browser))
                }
            }
        },
    )
}

fn user_label(user: &UserResponse) -> &str {
    user.credentials
        .iter()
        .find_map(|credential| match credential {
            HumanCredentialResponse::Password { username, .. } => Some(username.as_ref()),
            HumanCredentialResponse::Nostr { .. } => None,
        })
        .unwrap_or("Account")
}

fn role_choices(principal: &AdminPrincipal, selected: UserRole) -> Markup {
    html! {
        option value="publisher" selected[selected == UserRole::Publisher] { "Publisher" }
        @if principal.allows(AdminScope::RoleAssign) {
            option value="administrator" selected[selected == UserRole::Administrator] { "Administrator" }
            option value="owner" selected[selected == UserRole::Owner] { "Owner" }
        }
    }
}

fn create_user_forms(
    security: &AdminSecurityState,
    principal: &AdminPrincipal,
    browser: &BrowserFormSession,
) -> Markup {
    html! {
        @if security.providers.accepts(HumanLoginProvider::Password) {
            form method="post" action="/admin/users" {
                h3 { "Create with a password" }
                (mutation_fields(browser, None))
                input type="hidden" name="provider" value="password";
                label for="new-password-role" { "Role" }
                select id="new-password-role" name="role" required { (role_choices(principal, UserRole::Publisher)) }
                (password_inputs("new-user", ""))
                button type="submit" { "Create user" }
            }
        }
        @if security.providers.accepts(HumanLoginProvider::Nostr) {
            form method="post" action="/admin/users" {
                h3 { "Create with a Nostr key" }
                (mutation_fields(browser, None))
                input type="hidden" name="provider" value="nostr";
                label for="new-nostr-role" { "Role" }
                select id="new-nostr-role" name="role" required { (role_choices(principal, UserRole::Publisher)) }
                (nostr_input("new-user-public-key", ""))
                button type="submit" { "Create user" }
            }
        }
    }
}

async fn show_user(
    request_id: RequestId,
    Extension(security): Extension<AdminSecurityState>,
    principal: AdminPrincipal,
    browser: BrowserFormSession,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let user_id = match user_path(path, request_id) {
        Ok(user_id) => user_id,
        Err(response) => return mutation_response(response, "/admin/users", request_id),
    };
    let user = match security.store.user(user_id).await {
        Ok(Some(user)) => user,
        Ok(None) => return mutation_response(not_found(request_id), "/admin/users", request_id),
        Err(error) => {
            return mutation_response(load_problem(error, request_id), "/admin/users", request_id);
        }
    };
    let credentials = match security.store.user_credentials(user_id).await {
        Ok(Some(credentials)) => credentials,
        Ok(None) => return mutation_response(not_found(request_id), "/admin/users", request_id),
        Err(error) => {
            return mutation_response(load_problem(error, request_id), "/admin/users", request_id);
        }
    };
    let can_manage = user.scopes().is_subset(&principal.scopes);
    let user = user_response(&user, credentials);
    let selected_role = [
        UserRole::Owner,
        UserRole::Administrator,
        UserRole::Publisher,
    ]
    .into_iter()
    .find(|role| user.roles.contains(role))
    .unwrap_or(UserRole::Publisher);
    let fresh = browser.session.is_active_at(OffsetDateTime::now_utc())
        && OffsetDateTime::now_utc() < browser.session.fresh_until;
    page_response(
        StatusCode::OK,
        "User account",
        PageKind::Authenticated,
        html! {
            section class="panel" {
                h1 { (user_label(&user)) }
                p { "User ID: " code { (user.user_id) } }
                p { "Status: " (user.status.as_str()) " · Version: " (user.version) }
                p { "Roles: " @for role in &user.roles { code { (role.as_str()) } " " } }
                p { "Maincopy preserves at least one enabled Owner and a usable login credential for every enabled account." }
                @if !can_manage { p { "This account has authority beyond your current role. An Owner must manage it." } }
                @if !fresh { p class="notice" { "Account changes require a recent sign-in. " a href="/admin/login" { "Sign in again" } } }
                fieldset disabled[!fresh || !can_manage] {
                    legend { "Account access" }
                    form method="post" action=(format!("/admin/users/{user_id}/status")) {
                        (mutation_fields(&browser, Some(user.version)))
                        @match user.status {
                            UserStatus::Enabled => {
                                input type="hidden" name="status" value="disabled";
                                p { "Disabling an account revokes its browser sessions and agent credentials." }
                                button type="submit" { "Disable user" }
                            }
                            UserStatus::Disabled => {
                                input type="hidden" name="status" value="enabled";
                                p { "An enabled account must have a usable login credential." }
                                button type="submit" { "Enable user" }
                            }
                        }
                    }
                    @if principal.allows(AdminScope::RoleAssign) {
                        form method="post" action=(format!("/admin/users/{user_id}/roles")) {
                            (mutation_fields(&browser, Some(user.version)))
                            label for="role" { "Replace assigned roles" }
                            select name="role" id="role" required {
                                (role_choices(&principal, selected_role))
                            }
                            button type="submit" { "Save role" }
                        }
                    }
                }
            }
            section class="panel" {
                h2 { "Login credentials" }
                @if user.credentials.is_empty() { p { "No login credentials are configured." } }
                p { "Replacing or removing a login credential signs this user out on all devices." }
                fieldset disabled[!fresh || !can_manage] {
                    legend { "Manage sign-in" }
                    (credential_forms(&user, &security, &browser))
                }
            }
            p { a href="/admin/users" { "Return to users" } }
        },
    )
}

fn mutation_fields(browser: &BrowserFormSession, expected_version: Option<u64>) -> Markup {
    html! {
        input type="hidden" name="_csrf" value=(browser.csrf_token.expose_secret());
        input type="hidden" name="operation_id" value=(Uuid::new_v4());
        @if let Some(version) = expected_version {
            input type="hidden" name="expected_version" value=(version);
        }
    }
}

fn credential_forms(
    user: &UserResponse,
    security: &AdminSecurityState,
    browser: &BrowserFormSession,
) -> Markup {
    let password = user
        .credentials
        .iter()
        .find_map(|credential| match credential {
            HumanCredentialResponse::Password {
                username, version, ..
            } => Some((username.as_ref(), *version)),
            HumanCredentialResponse::Nostr { .. } => None,
        });
    let nostr = user
        .credentials
        .iter()
        .find_map(|credential| match credential {
            HumanCredentialResponse::Nostr {
                public_key,
                version,
                ..
            } => Some((public_key.as_ref(), *version)),
            HumanCredentialResponse::Password { .. } => None,
        });
    html! {
        @if security.providers.accepts(HumanLoginProvider::Password) {
            form method="post" action=(format!("/admin/users/{}/password", user.user_id)) {
                h3 { "Password" }
                (mutation_fields(browser, password.map(|(_, version)| version)))
                (password_inputs("account", password.map_or("", |(username, _)| username)))
                button type="submit" { "Save password" }
            }
        }
        @if security.providers.accepts(HumanLoginProvider::Nostr) {
            form method="post" action=(format!("/admin/users/{}/nostr", user.user_id)) {
                h3 { "Nostr login key" }
                (mutation_fields(browser, nostr.map(|(_, version)| version)))
                (nostr_input("account-public-key", nostr.map_or("", |(public_key, _)| public_key)))
                button type="submit" { "Save public key" }
            }
        }
        @for credential in &user.credentials {
            @let (provider, version) = match credential {
                HumanCredentialResponse::Password { version, .. } => (HumanLoginProvider::Password, *version),
                HumanCredentialResponse::Nostr { version, .. } => (HumanLoginProvider::Nostr, *version),
            };
            form method="post" action=(format!("/admin/users/{}/credentials/{}/remove", user.user_id, provider.as_str())) {
                (mutation_fields(browser, Some(version)))
                label {
                    input type="checkbox" name="confirm" value="true" required;
                    "Remove the " (provider.as_str()) " login credential at version " (version)
                }
                button type="submit" { "Remove credential" }
            }
        }
    }
}

fn password_inputs(prefix: &str, username: &str) -> Markup {
    // HTML counts UTF-16 units. Leave room for two units per accepted scalar.
    let maximum_units = MAX_PASSWORD_SCALARS * 2;
    html! {
        label for=(format!("{prefix}-username")) { "Username" }
        input name="username" id=(format!("{prefix}-username")) type="text" autocomplete="username" maxlength=(MAX_USERNAME_BYTES) required value=(username);
        label for=(format!("{prefix}-password")) { "New password" }
        input name="password" id=(format!("{prefix}-password")) type="password" autocomplete="new-password" minlength=(MIN_PASSWORD_SCALARS) maxlength=(maximum_units) required;
        label for=(format!("{prefix}-confirmation")) { "Confirm new password" }
        input name="confirmation" id=(format!("{prefix}-confirmation")) type="password" autocomplete="new-password" minlength=(MIN_PASSWORD_SCALARS) maxlength=(maximum_units) required;
        p class="muted" { "Use 15 to 128 characters. Passwords are never shown after submission." }
    }
}

fn nostr_input(id: &str, public_key: &str) -> Markup {
    html! {
        label for=(id) { "Public key (64 lowercase hexadecimal characters)" }
        input name="public_key" id=(id) type="text" minlength="64" maxlength="64" required value=(public_key);
    }
}

fn operation_headers(operation_id: Uuid) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        IDEMPOTENCY_KEY_HEADER,
        HeaderValue::from_str(&operation_id.to_string()).expect("UUIDs are valid header values"),
    );
    headers
}

fn invalid_form(error: FormRejection, request_id: RequestId) -> Response {
    mutation_error_response(error.status(), "/admin/users", request_id)
}

fn mutation_response(response: Response, location: &str, request_id: RequestId) -> Response {
    if response.status().is_success() {
        return redirect(location);
    }
    let mut page = mutation_error_response(response.status(), location, request_id);
    if let Some(retry_after) = response.headers().get(RETRY_AFTER) {
        page.headers_mut().insert(RETRY_AFTER, retry_after.clone());
    }
    page
}

fn password_mismatch(request_id: RequestId) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "Passwords did not match",
        "Enter the same new password in both fields.",
        request_id,
    )
}

async fn save_new_user(
    RequiredBrowserSession {
        request_id,
        security,
        session,
    }: RequiredBrowserSession,
    principal: AdminPrincipal,
    form: Result<Form<CreateUserForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(error) => return invalid_form(error, request_id),
    };
    let (operation_id, request) = match form.into_request() {
        Ok(request) => request,
        Err(_) => return password_mismatch(request_id),
    };
    match create_user_command(
        request_id,
        security,
        principal,
        Some(Extension(BrowserSessionContext { session })),
        operation_headers(operation_id),
        Ok(Json(request)),
    )
    .await
    {
        Ok(user) => redirect(&format!("/admin/users/{}", user.user_id)),
        Err(response) => mutation_response(*response, "/admin/users", request_id),
    }
}

async fn save_status(
    RequiredBrowserSession {
        request_id,
        security,
        session,
    }: RequiredBrowserSession,
    principal: AdminPrincipal,
    coordinator: State<PublicationCoordinatorHandle>,
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<UserStatusForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(error) => return invalid_form(error, request_id),
    };
    let response = set_user_status(
        request_id,
        Extension(security),
        principal,
        Some(Extension(BrowserSessionContext { session })),
        coordinator,
        operation_headers(form.operation_id),
        path,
        Ok(Json(SetUserStatusRequest {
            expected_version: form.expected_version,
            status: form.status,
        })),
    )
    .await;
    mutation_response(response, "/admin/users", request_id)
}

async fn save_role(
    RequiredBrowserSession {
        request_id,
        security,
        session,
    }: RequiredBrowserSession,
    principal: AdminPrincipal,
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<RoleForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(error) => return invalid_form(error, request_id),
    };
    let response = replace_user_roles(
        request_id,
        Extension(security),
        principal,
        Some(Extension(BrowserSessionContext { session })),
        operation_headers(form.operation_id),
        path,
        Ok(Json(ReplaceUserRolesRequest {
            expected_version: form.expected_version,
            roles: vec![form.role],
        })),
    )
    .await;
    mutation_response(response, "/admin/users", request_id)
}

async fn save_password(
    RequiredBrowserSession {
        request_id,
        security,
        session,
    }: RequiredBrowserSession,
    principal: AdminPrincipal,
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<PasswordForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(error) => return invalid_form(error, request_id),
    };
    let credential = match password_credential(form.username, form.password, form.confirmation) {
        Ok(credential) => credential,
        Err(_) => return password_mismatch(request_id),
    };
    let user_id = match user_path(path, request_id) {
        Ok(id) => id,
        Err(response) => return mutation_response(response, "/admin/users", request_id),
    };
    let request = match form.expected_version {
        None => PutHumanCredentialRequest::Create { credential },
        Some(expected_version) => PutHumanCredentialRequest::Replace {
            expected_version,
            credential,
        },
    };
    let response = put_human_credential(
        request_id,
        Extension(security),
        principal,
        Some(Extension(BrowserSessionContext { session })),
        operation_headers(form.operation_id),
        Ok(Path((user_id.to_string(), "password".to_owned()))),
        Ok(Json(request)),
    )
    .await;
    mutation_response(response, &format!("/admin/users/{user_id}"), request_id)
}

async fn save_nostr(
    RequiredBrowserSession {
        request_id,
        security,
        session,
    }: RequiredBrowserSession,
    principal: AdminPrincipal,
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<NostrForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(error) => return invalid_form(error, request_id),
    };
    let user_id = match user_path(path, request_id) {
        Ok(id) => id,
        Err(response) => return mutation_response(response, "/admin/users", request_id),
    };
    let credential = HumanCredentialInput::Nostr {
        public_key: form.public_key,
    };
    let request = match form.expected_version {
        None => PutHumanCredentialRequest::Create { credential },
        Some(expected_version) => PutHumanCredentialRequest::Replace {
            expected_version,
            credential,
        },
    };
    let response = put_human_credential(
        request_id,
        Extension(security),
        principal,
        Some(Extension(BrowserSessionContext { session })),
        operation_headers(form.operation_id),
        Ok(Path((user_id.to_string(), "nostr".to_owned()))),
        Ok(Json(request)),
    )
    .await;
    mutation_response(response, &format!("/admin/users/{user_id}"), request_id)
}

async fn remove_credential(
    RequiredBrowserSession {
        request_id,
        security,
        session,
    }: RequiredBrowserSession,
    principal: AdminPrincipal,
    path: Result<Path<(String, String)>, PathRejection>,
    form: Result<Form<RemoveCredentialForm>, FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(error) => return invalid_form(error, request_id),
    };
    if !form.confirm {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Removal needs confirmation",
            "Confirm the credential removal and submit the form again.",
            request_id,
        );
    }
    let response = remove_human_credential(
        request_id,
        Extension(security),
        principal,
        Some(Extension(BrowserSessionContext { session })),
        operation_headers(form.operation_id),
        path,
        Ok(Json(ExpectedVersionRequest {
            expected_version: form.expected_version,
        })),
    )
    .await;
    mutation_response(response, "/admin/users", request_id)
}
