use super::*;
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request as HttpRequest},
    routing::post,
};
use tower::ServiceExt as _;

fn removal_request(content_type: &str, body: impl Into<Body>) -> Request {
    HttpRequest::builder()
        .method(Method::POST)
        .uri("/email/unsubscribe/private-token")
        .header(CONTENT_TYPE, content_type)
        .body(body.into())
        .unwrap()
}

fn multipart(parts: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (name, value) in parts {
        body.push_str(&format!(
            "--boundary\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        ));
    }
    body.push_str("--boundary--\r\n");
    body
}

#[tokio::test]
async fn one_click_accepts_standard_encodings_without_browser_context() {
    for (content_type, body) in [
        (
            "application/x-www-form-urlencoded",
            "List-Unsubscribe=One-Click".to_owned(),
        ),
        (
            "multipart/form-data; boundary=boundary",
            multipart(&[("List-Unsubscribe", "One-Click")]),
        ),
    ] {
        let request = removal_request(content_type, body);
        assert!(!request.headers().contains_key(ORIGIN));
        assert!(!request.headers().contains_key("cookie"));
        assert!(matches!(
            removal_form(request).await.unwrap(),
            RemovalForm::OneClick {
                request: OneClick::Requested
            }
        ));
    }
    assert!(matches!(
        removal_form(removal_request(
            "application/x-www-form-urlencoded",
            "action=remove"
        ))
        .await
        .unwrap(),
        RemovalForm::Browser {
            action: RemovalAction::Remove
        }
    ));
}

#[tokio::test]
async fn removal_forms_reject_duplicate_mixed_unknown_and_file_parts() {
    for body in [
        "List-Unsubscribe=One-Click&List-Unsubscribe=One-Click",
        "List-Unsubscribe=One-Click&action=remove",
        "List-Unsubscribe=One-Click&extra=value",
        "List-Unsubscribe=wrong",
        "action=remove&action=remove",
        "action=other",
        "",
        "List-Unsubscribe=One-Click&address=reader%40example.com",
    ] {
        assert!(
            removal_form(removal_request("application/x-www-form-urlencoded", body))
                .await
                .is_err()
        );
    }
    for parts in [
        vec![
            ("List-Unsubscribe", "One-Click"),
            ("List-Unsubscribe", "One-Click"),
        ],
        vec![("List-Unsubscribe", "One-Click"), ("extra", "value")],
        vec![("wrong", "One-Click")],
        vec![("List-Unsubscribe", "wrong")],
        vec![],
    ] {
        assert!(
            removal_form(removal_request(
                "multipart/form-data; boundary=boundary",
                multipart(&parts)
            ))
            .await
            .is_err()
        );
    }
    let file = multipart(&[("List-Unsubscribe", "One-Click")]).replace(
        "name=\"List-Unsubscribe\"",
        "name=\"List-Unsubscribe\"; filename=\"payload.txt\"",
    );
    assert!(
        removal_form(removal_request(
            "multipart/form-data; boundary=boundary",
            file
        ))
        .await
        .is_err()
    );
    assert!(
        removal_form(removal_request("multipart/form-data", "invalid"))
            .await
            .is_err()
    );
    assert!(
        removal_form(removal_request(
            "application/json",
            r#"{"List-Unsubscribe":"One-Click"}"#
        ))
        .await
        .is_err()
    );
}

#[tokio::test]
async fn form_limit_and_private_error_headers_do_not_expose_request_secrets() {
    let router = Router::new()
        .route(
            "/email/unsubscribe/{token}",
            post(|request: Request| async { removal_form(request).await.map(|_| StatusCode::OK) }),
        )
        .layer(middleware::from_fn(bounded_body))
        .layer(middleware::from_fn(private_response));
    for (body, status) in [
        (
            "address=private-reader%40example.test".to_owned(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "x".repeat(MAX_FORM_BYTES + 1),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        let response = router
            .clone()
            .oneshot(removal_request("application/x-www-form-urlencoded", body))
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()[CACHE_CONTROL], "private, no-store");
        assert_eq!(response.headers()[REFERRER_POLICY], "no-referrer");
        assert_eq!(response.headers()[CONTENT_SECURITY_POLICY], PRIVATE_CSP);
        assert!(!response.headers().contains_key("location"));
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("private-token"));
        assert!(!text.contains("private-reader"));
    }
}

#[test]
fn origin_comparison_rejects_missing_duplicate_and_noncanonical_headers() {
    let mut headers = HeaderMap::new();
    assert!(!has_exact_header(&headers, ORIGIN, "https://example.com"));
    headers.insert(ORIGIN, HeaderValue::from_static("https://example.com"));
    assert!(has_exact_header(&headers, ORIGIN, "https://example.com"));
    headers.append(ORIGIN, HeaderValue::from_static("https://example.com"));
    assert!(!has_exact_header(&headers, ORIGIN, "https://example.com"));
    headers.insert(ORIGIN, HeaderValue::from_static("https://EXAMPLE.com"));
    assert!(!has_exact_header(&headers, ORIGIN, "https://example.com"));
}

#[cfg(unix)]
mod subscriber_routes {
    use std::{fs::OpenOptions, io::Write as _, os::unix::fs::OpenOptionsExt as _};

    use k256::schnorr::SigningKey;
    use maincopy_shared::auth::{AdminAuditEventId, InstanceId, UserId};
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        config::{
            DatabaseBusyTimeout, DatabaseConfigurationView, DatabaseReadPoolSize,
            DatabaseWriterQueueCapacity,
        },
        database,
        domain::{
            auth::{
                NostrPublicKey,
                store::{BootstrapIdentity, ConfiguredLoginProviders, NewHumanCredential},
            },
            mail::{
                config::{MailConfiguration, MailConfigurationCandidate},
                control::EncodedControlToken,
                runtime::{MailStartupError, prepare_mail},
                subscriber::{
                    BeginFeedbackRun, ClaimConfirmation, ConfirmationHandle, DeliveryAdmission,
                    FeedbackObservation, RecordFeedbackObservation, SubscriberMode,
                    SubscriberPolicy,
                },
            },
        },
    };

    struct Fixture {
        root: tempfile::TempDir,
        state: PublicMailState,
        store: database::DatabaseStore,
        policy: SubscriberPolicy,
        feedback_run: Uuid,
        shutdown: CancellationToken,
        writer: JoinHandle<()>,
    }

    struct Invitation {
        enrollment: Uuid,
        generation: Uuid,
        nonce: Uuid,
        expires_at: OffsetDateTime,
        confirmation: EncodedControlToken,
        removal: EncodedControlToken,
    }

    impl Invitation {
        fn confirmation_command(&self) -> ConfirmEnrollment {
            ConfirmEnrollment {
                enrollment: self.enrollment,
                generation: self.generation,
                nonce_digest: SubscriberDigest::from_bytes(MailControls::confirmation_digest(
                    &self.nonce,
                )),
                expires_at: self.expires_at,
            }
        }

        fn removal_command(&self) -> ManageEnrollment {
            ManageEnrollment {
                enrollment: self.enrollment,
                generation: self.generation,
            }
        }
    }

    impl Fixture {
        async fn start() -> Self {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("state/maincopy.db");
            let database = database::bootstrap(DatabaseConfigurationView {
                path: &path,
                busy_timeout: DatabaseBusyTimeout::from_milliseconds(1000).unwrap(),
                writer_queue_capacity: DatabaseWriterQueueCapacity::new(32).unwrap(),
                read_pool_size: DatabaseReadPoolSize::new(2).unwrap(),
            })
            .await
            .unwrap();
            let (store, writer) = database.into_store(32);
            let shutdown = CancellationToken::new();
            let cancellation = shutdown.clone();
            let writer = tokio::spawn(async move {
                writer.run(cancellation).await.unwrap();
            });
            let instance = InstanceId::from_uuid(Uuid::new_v4());
            let key = SigningKey::from_bytes(&[7; 32]).unwrap();
            store
                .auth
                .bootstrap_identity(BootstrapIdentity {
                    instance_id: instance,
                    owner_user_id: UserId::from_uuid(Uuid::new_v4()),
                    credential: NewHumanCredential::Nostr {
                        public_key: NostrPublicKey::from_bytes(
                            key.verifying_key().to_bytes().into(),
                        )
                        .unwrap(),
                    },
                    configured_providers: ConfiguredLoginProviders::new(false, true).unwrap(),
                    occurred_at: OffsetDateTime::now_utc(),
                    audit_event_id: AdminAuditEventId::from_uuid(Uuid::new_v4()),
                })
                .await
                .unwrap();
            let control_path = root.path().join("control.key");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&control_path)
                .unwrap();
            file.write_all("7a".repeat(32).as_bytes()).unwrap();
            drop(file);
            let controls = Arc::new(MailControls::load(&control_path, instance).unwrap());
            let configuration = configuration(root.path(), "enabled");
            let public_policy = configuration.view().subscriptions.unwrap().clone();
            let credentials = SesCredentials::parse(
                br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"1234567890123456"}"#,
            )
            .unwrap();
            let mut credential_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(root.path().join("ses.json"))
                .unwrap();
            credential_file
                .write_all(
                    br#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"1234567890123456"}"#,
                )
                .unwrap();
            drop(credential_file);
            let state = PublicMailState::new(
                configuration,
                &credentials,
                public_policy,
                PublicationBaseUrl::parse("https://example.com").unwrap(),
                controls,
                store.subscribers.clone(),
            );
            let policy = SubscriberPolicy {
                configuration_binding: state.configuration_binding,
                mode: SubscriberMode::Enabled,
                max_daily_messages: 100,
                max_daily_confirmations: 100,
                max_campaign_recipients: 100,
            };
            state
                .subscribers
                .initialize_controls(state.controls.identity_binding(&state.origin))
                .await
                .unwrap();
            state.subscribers.set_policy(policy).await.unwrap();
            let run = state
                .subscribers
                .begin_feedback_run(BeginFeedbackRun {
                    provider_now: OffsetDateTime::now_utc(),
                    configuration_binding: state.configuration_binding,
                    source_binding: [6; 32],
                    retention_seconds: 1_209_600,
                })
                .await
                .unwrap();
            state
                .subscribers
                .record_feedback_observation(RecordFeedbackObservation {
                    configuration_binding: state.configuration_binding,
                    run_id: run.run_id,
                    source_binding: [6; 32],
                    retention_seconds: 1_209_600,
                    observation: FeedbackObservation::Observed {
                        provider_now: OffsetDateTime::now_utc(),
                        drained: true,
                    },
                })
                .await
                .unwrap();
            Self {
                root,
                state,
                store,
                policy,
                feedback_run: run.run_id,
                shutdown,
                writer,
            }
        }

        fn router(&self) -> Router {
            router(self.state.clone())
        }

        async fn queue(&self, address: &str) -> ConfirmationHandle {
            let address = EmailAddress::parse(address).unwrap();
            let command = RequestEnrollment {
                mailbox_digest: SubscriberDigest::from_bytes(
                    self.state.controls.mailbox_digest(&address),
                ),
                address,
                enrollment: Uuid::new_v4(),
                generation: Uuid::new_v4(),
                confirmation_attempt: Uuid::new_v4(),
                configuration_binding: self.state.configuration_binding,
            };
            let attempt_id = command.confirmation_attempt;
            assert_eq!(
                self.state
                    .subscribers
                    .request_enrollment(command)
                    .await
                    .unwrap(),
                EnrollmentRequestResult::Queued
            );
            self.state
                .subscribers
                .queued_confirmations(self.state.configuration_binding, 100)
                .await
                .unwrap()
                .into_iter()
                .find(|item| item.attempt_id == attempt_id)
                .unwrap()
        }

        async fn invitation(&self, address: &str) -> Invitation {
            let handle = self.queue(address).await;
            let nonce = Uuid::new_v4();
            let now = OffsetDateTime::now_utc();
            let expires_at =
                OffsetDateTime::from_unix_timestamp(now.unix_timestamp() + 3600).unwrap();
            let admission = self
                .state
                .subscribers
                .claim_confirmation(ClaimConfirmation {
                    attempt_id: handle.attempt_id,
                    nonce_digest: SubscriberDigest::from_bytes(MailControls::confirmation_digest(
                        &nonce,
                    )),
                    expires_at,
                    configuration_binding: self.state.configuration_binding,
                })
                .await
                .unwrap();
            assert!(matches!(admission, DeliveryAdmission::Ready(_)));
            drop(admission); // No provider request occurs in this fixture.
            let confirmation = self
                .state
                .controls
                .issue(
                    ControlClaims::Confirm {
                        enrollment: handle.enrollment,
                        generation: handle.generation,
                        confirmation_nonce: nonce,
                        expires_at,
                    },
                    now,
                )
                .unwrap();
            let removal = self
                .state
                .controls
                .issue(
                    ControlClaims::Manage {
                        enrollment: handle.enrollment,
                        generation: handle.generation,
                    },
                    now,
                )
                .unwrap();
            Invitation {
                enrollment: handle.enrollment,
                generation: handle.generation,
                nonce,
                expires_at,
                confirmation,
                removal,
            }
        }

        async fn stop(self) {
            let Self {
                root,
                state,
                store,
                shutdown,
                writer,
                ..
            } = self;
            drop(state);
            drop(store);
            shutdown.cancel();
            writer.await.unwrap();
            drop(root);
        }
    }

    fn configuration(root: &std::path::Path, mode: &str) -> SesMailConfiguration {
        let source = format!(
            r#"
mode = "ses"
sender = "newsletter@example.com"
region = "us-east-1"
configuration_set = "newsletter"
credential_file = "ses.json"
control_signing_key_file = "control.key"
max_daily_messages = 100
max_daily_confirmation_messages = 100
max_campaign_recipients = 100
[subscriptions]
mode = "{mode}"
operator_name = "An <operator>"
postal_address = "PO Box 123, Example City"
purpose = "Send reviewed article announcements."
privacy_url = "https://example.com/privacy"
contact_address = "contact@example.com"
[feedback]
queue_url = "https://sqs.us-east-1.amazonaws.com/123456789012/newsletter"
topic_arn = "arn:aws:sns:us-east-1:123456789012:newsletter"
"#
        );
        let candidate: MailConfigurationCandidate = toml::from_str(&source).unwrap();
        let MailConfiguration::Ses(configuration) = candidate.validate(root).unwrap() else {
            panic!("fixture configures SES")
        };
        *configuration
    }

    fn request(method: Method, path: &str, body: impl Into<Body>, browser: bool) -> Request {
        let mut builder = HttpRequest::builder()
            .method(method)
            .uri(path)
            .header(HOST, "example.com")
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded");
        if browser {
            builder = builder.header(ORIGIN, "https://example.com");
        }
        builder.body(body.into()).unwrap()
    }

    async fn response_body(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), 16 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    fn subscribe_body(address: &str) -> String {
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("address", address)
            .append_pair("consent", "yes")
            .finish()
    }

    #[tokio::test]
    async fn signup_page_observes_durable_pause_and_feedback_loss_without_rebuilding_routes() {
        let fixture = Fixture::start().await;
        let app = fixture.router();
        fixture
            .state
            .subscribers
            .set_policy(SubscriberPolicy {
                mode: SubscriberMode::Paused,
                ..fixture.policy
            })
            .await
            .unwrap();
        let paused = app
            .clone()
            .oneshot(request(Method::GET, SUBSCRIBE_ROUTE, Body::empty(), false))
            .await
            .unwrap();
        let paused = response_body(paused).await;
        assert!(paused.contains("New subscriptions are paused"));
        assert!(!paused.contains("name=\"address\""));
        let refused = app
            .clone()
            .oneshot(request(
                Method::POST,
                SUBSCRIBE_ROUTE,
                subscribe_body("reader@example.net"),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            response_body(refused)
                .await
                .contains("New subscriptions are paused")
        );
        fixture
            .state
            .subscribers
            .set_policy(fixture.policy)
            .await
            .unwrap();
        fixture
            .state
            .subscribers
            .record_feedback_health(
                fixture.state.configuration_binding,
                FeedbackHealth::Unavailable,
            )
            .await
            .unwrap();
        let unavailable = app
            .oneshot(request(Method::GET, SUBSCRIBE_ROUTE, Body::empty(), false))
            .await
            .unwrap();
        assert!(
            !response_body(unavailable)
                .await
                .contains("name=\"address\"")
        );
        assert_eq!(
            fixture
                .state
                .subscribers
                .status()
                .await
                .unwrap()
                .addressed_enrollments,
            0
        );
        fixture.stop().await;
    }

    #[tokio::test]
    async fn signup_is_explicit_generic_and_durably_queued_without_sending() {
        let fixture = Fixture::start().await;
        let app = fixture.router();
        let page = app
            .clone()
            .oneshot(request(Method::GET, SUBSCRIBE_ROUTE, Body::empty(), false))
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        let page = response_body(page).await;
        assert!(page.contains("An &lt;operator&gt;"));
        assert!(page.contains("name=\"consent\""));
        assert!(page.contains("Privacy and retention policy"));
        for body in [
            "address=reader%40example.net",
            "address=reader%40example.net&consent=no",
            "address=reader%40example.net&consent=yes&extra=1",
            "address=reader%40example.net&address=other%40example.net&consent=yes",
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request(Method::POST, SUBSCRIBE_ROUTE, body, true))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
        assert!(
            fixture
                .state
                .subscribers
                .queued_confirmations(fixture.state.configuration_binding, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let initial = app
            .clone()
            .oneshot(request(
                Method::POST,
                SUBSCRIBE_ROUTE,
                subscribe_body("reader@example.net"),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(initial.status(), StatusCode::ACCEPTED);
        let initial = response_body(initial).await;
        let repeated = app
            .clone()
            .oneshot(request(
                Method::POST,
                SUBSCRIBE_ROUTE,
                subscribe_body("READER@EXAMPLE.NET"),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(repeated.status(), StatusCode::ACCEPTED);
        assert_eq!(initial, response_body(repeated).await);
        assert!(!initial.contains("reader@"));
        assert_eq!(
            fixture
                .state
                .subscribers
                .queued_confirmations(fixture.state.configuration_binding, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        drop(app);
        fixture.stop().await;
    }

    #[tokio::test]
    async fn token_gets_are_scanner_safe_and_post_confirmation_consumes_current_nonce() {
        let fixture = Fixture::start().await;
        let invitation = fixture.invitation("scanner@example.net").await;
        let app = fixture.router();
        for (prefix, token) in [
            ("confirm", invitation.confirmation.as_str()),
            ("unsubscribe", invitation.removal.as_str()),
        ] {
            let path = format!("/email/{prefix}/{token}");
            for method in [Method::GET, Method::HEAD] {
                let response = app
                    .clone()
                    .oneshot(request(method, &path, Body::empty(), false))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()[REFERRER_POLICY], "no-referrer");
                let body = response_body(response).await;
                assert!(!body.contains(token));
                assert!(!body.contains("scanner@example.net"));
            }
        }
        assert_eq!(
            fixture
                .state
                .subscribers
                .confirm(invitation.confirmation_command())
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        assert_eq!(
            fixture
                .state
                .subscribers
                .remove(invitation.removal_command())
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        let second = fixture.invitation("confirmed@example.net").await;
        let path = format!("/email/confirm/{}", second.confirmation.as_str());
        assert_eq!(
            app.clone()
                .oneshot(request(Method::POST, &path, "action=confirm", false))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Method::POST, &path, "action=confirm", true))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            fixture
                .state
                .subscribers
                .confirm(second.confirmation_command())
                .await
                .unwrap(),
            ControlOutcome::Unchanged
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Method::POST, &path, "action=confirm", true))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        drop(app);
        fixture.stop().await;
    }

    #[tokio::test]
    async fn one_click_removes_while_paused_and_old_links_cannot_remove_a_new_generation() {
        let fixture = Fixture::start().await;
        let first = fixture.invitation("returning@example.net").await;
        assert_eq!(
            fixture
                .state
                .subscribers
                .confirm(first.confirmation_command())
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        let mut paused = fixture.state.clone();
        paused.policy = configuration(fixture.root.path(), "paused")
            .view()
            .subscriptions
            .unwrap()
            .clone();
        fixture
            .state
            .subscribers
            .set_policy(SubscriberPolicy {
                mode: SubscriberMode::Paused,
                ..fixture.policy
            })
            .await
            .unwrap();
        let app = router(paused);
        let old_path = format!("/email/unsubscribe/{}", first.removal.as_str());
        assert_eq!(
            app.clone()
                .oneshot(request(
                    Method::POST,
                    SUBSCRIBE_ROUTE,
                    subscribe_body("paused@example.net"),
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(request(
                    Method::POST,
                    &old_path,
                    "List-Unsubscribe=One-Click",
                    false,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!response.headers().contains_key("location"));
            assert!(!response.headers().contains_key("set-cookie"));
        }
        assert_eq!(
            fixture
                .state
                .subscribers
                .remove(first.removal_command())
                .await
                .unwrap(),
            ControlOutcome::Unchanged
        );
        fixture
            .state
            .subscribers
            .set_policy(fixture.policy)
            .await
            .unwrap();
        fixture
            .state
            .subscribers
            .record_feedback_observation(RecordFeedbackObservation {
                configuration_binding: fixture.state.configuration_binding,
                run_id: fixture.feedback_run,
                source_binding: [6; 32],
                retention_seconds: 1_209_600,
                observation: FeedbackObservation::Observed {
                    provider_now: OffsetDateTime::now_utc(),
                    drained: true,
                },
            })
            .await
            .unwrap();
        let second = fixture.invitation("returning@example.net").await;
        assert_ne!(first.generation, second.generation);
        assert_eq!(
            fixture
                .state
                .subscribers
                .confirm(second.confirmation_command())
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        assert_eq!(
            app.clone()
                .oneshot(request(
                    Method::POST,
                    &old_path,
                    "List-Unsubscribe=One-Click",
                    false
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            fixture
                .state
                .subscribers
                .remove(second.removal_command())
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        drop(app);
        fixture.stop().await;
    }

    #[tokio::test]
    async fn public_controls_reject_foreign_host_origin_purpose_and_expired_tokens() {
        let fixture = Fixture::start().await;
        let invitation = fixture.invitation("security@example.net").await;
        let app = fixture.router();
        let removal_path = format!("/email/unsubscribe/{}", invitation.removal.as_str());
        let mut foreign_host = request(
            Method::POST,
            &removal_path,
            "List-Unsubscribe=One-Click",
            false,
        );
        foreign_host
            .headers_mut()
            .insert(HOST, HeaderValue::from_static("foreign.example"));
        assert_eq!(
            app.clone().oneshot(foreign_host).await.unwrap().status(),
            StatusCode::MISDIRECTED_REQUEST
        );
        let mut foreign_origin = request(Method::POST, &removal_path, "action=remove", true);
        foreign_origin
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static("https://foreign.example"));
        assert_eq!(
            app.clone().oneshot(foreign_origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let wrong_purpose = format!("/email/unsubscribe/{}", invitation.confirmation.as_str());
        assert_eq!(
            app.clone()
                .oneshot(request(
                    Method::POST,
                    &wrong_purpose,
                    "List-Unsubscribe=One-Click",
                    false
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let now = OffsetDateTime::now_utc();
        let expired = fixture
            .state
            .controls
            .issue(
                ControlClaims::Confirm {
                    enrollment: invitation.enrollment,
                    generation: invitation.generation,
                    confirmation_nonce: invitation.nonce,
                    expires_at: now - time::Duration::hours(1),
                },
                now - time::Duration::hours(2),
            )
            .unwrap();
        let expired_path = format!("/email/confirm/{}", expired.as_str());
        assert_eq!(
            app.clone()
                .oneshot(request(Method::POST, &expired_path, "action=confirm", true))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            fixture
                .state
                .subscribers
                .confirm(invitation.confirmation_command())
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        assert_eq!(
            fixture
                .state
                .subscribers
                .remove(invitation.removal_command())
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        drop(app);
        fixture.stop().await;
    }

    #[tokio::test]
    async fn runtime_keeps_existing_removal_controls_and_rejects_an_origin_that_strands_them() {
        let fixture = Fixture::start().await;
        let invitation = fixture.invitation("runtime-reader@example.net").await;
        assert_eq!(
            fixture
                .state
                .subscribers
                .confirm(invitation.confirmation_command())
                .await
                .unwrap(),
            ControlOutcome::Changed
        );
        let disabled = prepare_mail(
            &MailConfiguration::Disabled,
            &fixture.store,
            fixture.state.origin.clone(),
        )
        .await;
        assert!(matches!(disabled, Err(MailStartupError::RemovalRequired)));
        let paused = MailConfiguration::Ses(Box::new(configuration(fixture.root.path(), "paused")));
        let moved = prepare_mail(
            &paused,
            &fixture.store,
            PublicationBaseUrl::parse("https://moved.example/").unwrap(),
        )
        .await;
        assert!(matches!(
            moved,
            Err(MailStartupError::SubscriberMutation(_))
        ));
        let prepared = prepare_mail(&paused, &fixture.store, fixture.state.origin.clone())
            .await
            .unwrap();
        assert_eq!(
            fixture
                .store
                .subscribers
                .status()
                .await
                .unwrap()
                .policy
                .unwrap()
                .mode,
            SubscriberMode::Paused
        );
        assert!(prepared.dispatcher.is_some());
        let app = prepared.public_routes.unwrap();
        let response = app
            .oneshot(request(
                Method::POST,
                &format!("/email/unsubscribe/{}", invitation.removal.as_str()),
                "action=remove",
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            fixture
                .store
                .subscribers
                .status()
                .await
                .unwrap()
                .addressed_enrollments,
            0
        );
        let disabled = prepare_mail(
            &MailConfiguration::Disabled,
            &fixture.store,
            fixture.state.origin.clone(),
        )
        .await
        .unwrap();
        assert!(disabled.public_routes.is_none());
        fixture.stop().await;
    }
}
