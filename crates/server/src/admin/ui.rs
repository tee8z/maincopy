use axum::{
    Extension, Form, Router,
    body::Body,
    extract::{DefaultBodyLimit, Request},
    http::{
        HeaderValue, Method, StatusCode,
        header::{LOCATION, RETRY_AFTER},
    },
    middleware::{self, Next},
    response::{Html, IntoResponse as _, Response},
    routing::{get, post},
};
use maincopy_shared::{auth::HumanLoginProvider, auth_api::SecretString};
use maud::{DOCTYPE, Markup, html};
use serde::Deserialize;

use super::{
    assets,
    request_id::RequestId,
    security::{
        self, AdminSecurityState, RequiredBrowserSession, TrustedLoginRequest,
        browser_session_router,
    },
};

const MAX_LOGIN_FORM_BYTES: usize = 8 * 1024;
const MAX_LOGOUT_FORM_BYTES: usize = 1024;
const MAX_ADMIN_PAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) enum PageKind {
    Login,
    Authenticated,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordLoginForm {
    username: Box<str>,
    password: SecretString,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogoutForm {
    #[serde(rename = "_csrf")]
    _csrf: SecretString,
}

pub(super) fn public_router() -> Router {
    Router::new()
        .route("/admin/login", get(show_login).post(submit_password_login))
        .route("/admin/assets/{digest}/{name}", get(assets::get))
        .layer(DefaultBodyLimit::max(MAX_LOGIN_FORM_BYTES))
}

pub(super) fn protected_router(security: &AdminSecurityState) -> Router {
    browser_session_router(
        Router::new()
            .route("/admin/logout", post(logout))
            .layer(DefaultBodyLimit::max(MAX_LOGOUT_FORM_BYTES)),
        security,
    )
    .layer(middleware::from_fn(adapt_security_response))
}

async fn show_login(Extension(security): Extension<AdminSecurityState>) -> Response {
    login_response(&security, StatusCode::OK, None)
}

async fn submit_password_login(
    TrustedLoginRequest {
        request_id,
        security,
    }: TrustedLoginRequest,
    form: Result<Form<PasswordLoginForm>, axum::extract::rejection::FormRejection>,
) -> Response {
    let form = match form {
        Ok(Form(form)) => form,
        Err(rejection) => {
            let (status, message) = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "The sign-in form was too large. Please try again.",
                )
            } else {
                (
                    StatusCode::BAD_REQUEST,
                    "The sign-in form was not valid. Please try again.",
                )
            };
            return login_response(&security, status, Some(message));
        }
    };
    if !security.providers.accepts(HumanLoginProvider::Password) {
        return login_response(
            &security,
            StatusCode::FORBIDDEN,
            Some("Password sign-in is not enabled for this server."),
        );
    }

    let mut response =
        security::create_password_session(&security, &form.username, form.password, request_id)
            .await;
    if response.status() == StatusCode::CREATED {
        *response.status_mut() = StatusCode::SEE_OTHER;
        response
            .headers_mut()
            .insert(LOCATION, axum::http::HeaderValue::from_static("/admin"));
        response
            .headers_mut()
            .remove(axum::http::header::CONTENT_TYPE);
        *response.body_mut() = Body::empty();
        return response;
    }

    let retry_after = response.headers().get(RETRY_AFTER).cloned();
    let (status, message) = match response.status() {
        StatusCode::UNAUTHORIZED => (
            StatusCode::UNAUTHORIZED,
            "The username or password was not accepted.",
        ),
        StatusCode::TOO_MANY_REQUESTS => (
            StatusCode::TOO_MANY_REQUESTS,
            "Sign-in is busy. Wait a moment and try again.",
        ),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Sign-in is temporarily unavailable. Try again shortly.",
        ),
    };
    let mut response = login_response(&security, status, Some(message));
    if let Some(retry_after) = retry_after {
        response.headers_mut().insert(RETRY_AFTER, retry_after);
    }
    response
}

async fn logout(
    RequiredBrowserSession {
        request_id,
        security,
        session,
    }: RequiredBrowserSession,
    form: Result<Form<LogoutForm>, axum::extract::rejection::FormRejection>,
) -> Response {
    let form = match form {
        Ok(Form(form)) => form,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Sign-out did not complete",
                "The sign-out confirmation was too large. Return to posts and try again.",
                request_id,
            );
        }
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Sign-out did not complete",
                "The sign-out confirmation was not valid. Return to posts and try again.",
                request_id,
            );
        }
    };
    if form._csrf.expose_secret().is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Sign-out did not complete",
            "The sign-out confirmation was not valid. Return to posts and try again.",
            request_id,
        );
    }

    let mut response = security::revoke_browser_session(&security, session, request_id).await;
    if response.status() == StatusCode::OK {
        *response.status_mut() = StatusCode::SEE_OTHER;
        response
            .headers_mut()
            .insert(LOCATION, HeaderValue::from_static("/admin/login"));
        response
            .headers_mut()
            .remove(axum::http::header::CONTENT_TYPE);
        *response.body_mut() = Body::empty();
        return response;
    }

    let status = response.status();
    error_response(
        status,
        "Sign-out did not complete",
        "The session could not be revoked. Return to posts and try again.",
        request_id,
    )
}

pub(crate) async fn adapt_security_response(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let request_id = request.extensions().get::<RequestId>().copied();
    let response = next.run(request).await;
    match response.status() {
        StatusCode::UNAUTHORIZED => redirect("/admin/login"),
        StatusCode::FORBIDDEN if method != Method::HEAD => {
            request_id.map_or(response, |request_id| {
                error_response(
                    StatusCode::FORBIDDEN,
                    "Request denied",
                    "The request could not be authorized. Return to posts and retry from a current page.",
                    request_id,
                )
            })
        }
        _ => response,
    }
}

pub(crate) fn redirect(location: &str) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    response.headers_mut().insert(
        LOCATION,
        HeaderValue::from_str(location).expect("typed admin paths form valid redirect locations"),
    );
    response
}

fn login_response(
    security: &AdminSecurityState,
    status: StatusCode,
    error: Option<&str>,
) -> Response {
    let password_enabled = security.providers.accepts(HumanLoginProvider::Password);
    let nostr_enabled = security.providers.accepts(HumanLoginProvider::Nostr);
    let content = html! {
        section class="panel" {
            h1 { "Sign in to Maincopy" }
            p class="muted" { "Review and publish the exact content loaded by this server." }
            @if let Some(message) = error {
                p class="error" role="alert" { (message) }
            }
            @if password_enabled {
                form method="post" action="/admin/login" {
                    label for="username" { "Username" }
                    input id="username" name="username" type="text" autocomplete="username"
                        required maxlength="128";
                    label for="password" { "Password" }
                    input id="password" name="password" type="password"
                        autocomplete="current-password" required maxlength="4096";
                    button type="submit" { "Sign in" }
                }
            } @else {
                p { "Password sign-in is not enabled." }
            }
            @if nostr_enabled {
                p class="muted" {
                    "Nostr authentication is enabled through the administration API; "
                    "this first browser screen uses password sign-in."
                }
            }
        }
    };
    page_response(status, "Sign in", PageKind::Login, content)
}

pub(crate) fn page_response(
    status: StatusCode,
    title: &str,
    kind: PageKind,
    content: Markup,
) -> Response {
    let full_title = format!("{title} — Maincopy administration");
    let stylesheet = assets::stylesheet_path();
    let document = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (full_title) }
                link rel="stylesheet" href=(stylesheet);
            }
            body {
                main {
                    @if matches!(kind, PageKind::Authenticated) {
                        header {
                            a href="/admin" { strong { "Maincopy" } }
                            span class="muted" { "Private administration" }
                        }
                    }
                    (content)
                }
            }
        }
    };
    let document = document.into_string();
    if document.len() > MAX_ADMIN_PAGE_BYTES {
        let mut response = Html(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Administration page unavailable</title></head><body><main><h1>Administration page unavailable</h1><p>The page exceeded the safe rendering limit.</p></main></body></html>",
        )
        .into_response();
        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        return response;
    }
    let mut response = Html(document).into_response();
    *response.status_mut() = status;
    response
}

pub(crate) fn error_response(
    status: StatusCode,
    title: &str,
    message: &str,
    request_id: RequestId,
) -> Response {
    page_response(
        status,
        title,
        PageKind::Authenticated,
        html! {
            section class="error" role="alert" {
                h1 { (title) }
                p { (message) }
                p class="muted" { "Request ID: " code { (request_id) } }
            }
            nav class="actions" aria-label="Recovery actions" {
                a class="button" href="/admin" { "Return to posts" }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    #[tokio::test]
    async fn page_shell_escapes_untrusted_content() {
        let response = page_response(
            StatusCode::BAD_REQUEST,
            "Bad <title>",
            PageKind::Authenticated,
            html! { p { "<script>alert(1)</script>" } },
        );
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = String::from_utf8(
            to_bytes(response.into_body(), 16 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("Bad &lt;title&gt;"));
        assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!body.contains("<script>alert(1)</script>"));
    }
}
