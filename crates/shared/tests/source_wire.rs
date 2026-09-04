use maincopy_shared::source::{
    GIT_SHA1_SOURCE_COMMIT_PREFIX, SOURCE_PATH, SOURCE_SYNCS_PATH, SourceStatusResponse,
    SourceSyncFailureCode, SourceSyncId, SourceSyncOutcome, SourceSyncRequestOrigin,
    SourceSyncResource, SourceSyncStage,
};
use serde_json::{Value, json};
use uuid::Uuid;

const SOURCE_COMMIT: &str = "git-sha1:abababababababababababababababababababab";
const CONTENT_DIGEST: &str =
    "content-b3-v1-cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

fn source_sync(
    stage: &str,
    outcome: Option<&str>,
    source_commit: Option<&str>,
    content_digest: Option<&str>,
    failure_code: Option<&str>,
) -> Value {
    json!({
        "source_sync_id": "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
        "configuration_version": 1,
        "request_origin": "manual",
        "stage": stage,
        "outcome": outcome,
        "source_commit": source_commit,
        "content_digest": content_digest,
        "failure_code": failure_code,
        "version": 6,
        "requested_at": "2026-09-04T12:00:00Z",
        "updated_at": "2026-09-04T12:00:01Z",
        "finished_at": outcome.map(|_| "2026-09-04T12:00:01Z")
    })
}

fn assert_sync_rejected(value: Value, case: &str) {
    assert!(
        serde_json::from_value::<SourceSyncResource>(value).is_err(),
        "accepted {case}"
    );
}

fn managed_source_status(
    installed_commit: Option<&str>,
    content_digest: Option<&str>,
    active_sync: Option<Value>,
    latest_sync: Option<Value>,
) -> Value {
    json!({
        "mode": "managed_git",
        "configuration": {
            "remote": {
                "user": "git",
                "host": "git.example.test",
                "port": 22,
                "repository_path": "publisher/site.git"
            },
            "branch": "main",
            "content_subdirectory": ".",
            "credential_name": "deploy-key",
            "poll_interval_seconds": 60,
            "version": 1,
            "updated_at": "2026-09-04T12:00:00Z"
        },
        "installed_commit": installed_commit,
        "content_digest": content_digest,
        "active_sync": active_sync,
        "latest_sync": latest_sync,
        "next_poll_at": "2026-09-04T12:01:00Z"
    })
}

#[test]
fn source_paths_and_sync_identifiers_have_stable_encodings() {
    assert_eq!(SOURCE_PATH, "/api/admin/v1/source");
    assert_eq!(SOURCE_SYNCS_PATH, "/api/admin/v1/source-syncs");
    let identifier =
        SourceSyncId::from_uuid(Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap());
    assert_eq!(
        serde_json::to_value(identifier).unwrap(),
        json!(identifier.to_string())
    );
}

#[test]
fn source_state_enums_have_stable_closed_wire_names() {
    for (value, encoded) in [
        (SourceSyncStage::Queued, "queued"),
        (SourceSyncStage::Fetching, "fetching"),
        (SourceSyncStage::ResolvingCommit, "resolving_commit"),
        (SourceSyncStage::PreparingCandidate, "preparing_candidate"),
        (SourceSyncStage::Compiling, "compiling"),
        (SourceSyncStage::Reloading, "reloading"),
    ] {
        assert_eq!(serde_json::to_value(value).unwrap(), json!(encoded));
        assert_eq!(SourceSyncStage::parse(encoded), Some(value));
    }
    for (value, encoded) in [
        (SourceSyncOutcome::Applied, "applied"),
        (SourceSyncOutcome::NoChange, "no_change"),
        (SourceSyncOutcome::Failed, "failed"),
        (SourceSyncOutcome::Cancelled, "cancelled"),
    ] {
        assert_eq!(serde_json::to_value(value).unwrap(), json!(encoded));
        assert_eq!(SourceSyncOutcome::parse(encoded), Some(value));
    }
    assert_eq!(SourceSyncRequestOrigin::parse("unknown"), None);
    assert_eq!(SourceSyncStage::parse("unknown"), None);
    assert_eq!(SourceSyncOutcome::parse("unknown"), None);
    assert_eq!(SourceSyncFailureCode::parse("unknown"), None);
}

#[test]
fn source_resources_reject_untyped_commit_and_content_identities_during_decode() {
    let resource = |source_commit: &str, content_digest: &str| {
        json!({
            "source_sync_id": "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
            "configuration_version": 1,
            "request_origin": "manual",
            "stage": "reloading",
            "outcome": "applied",
            "source_commit": source_commit,
            "content_digest": content_digest,
            "failure_code": null,
            "version": 6,
            "requested_at": "2026-09-04T12:00:00Z",
            "updated_at": "2026-09-04T12:00:01Z",
            "finished_at": "2026-09-04T12:00:01Z"
        })
    };
    let commit = format!("{GIT_SHA1_SOURCE_COMMIT_PREFIX}{}", "ab".repeat(20));
    let digest = format!("content-b3-v1-{}", "cd".repeat(32));
    assert!(serde_json::from_value::<SourceSyncResource>(resource(&commit, &digest)).is_ok());
    assert!(
        serde_json::from_value::<SourceSyncResource>(resource(&"ab".repeat(20), &digest)).is_err()
    );
    assert!(
        serde_json::from_value::<SourceSyncResource>(resource(&commit, &"cd".repeat(32))).is_err()
    );
}

#[test]
fn source_sync_wire_accepts_every_durable_lifecycle_shape() {
    for (stage, outcome, source_commit, content_digest, failure_code) in [
        ("queued", None, None, None, None),
        ("fetching", None, None, None, None),
        ("resolving_commit", None, None, None, None),
        ("preparing_candidate", None, Some(SOURCE_COMMIT), None, None),
        ("compiling", None, Some(SOURCE_COMMIT), None, None),
        (
            "reloading",
            None,
            Some(SOURCE_COMMIT),
            Some(CONTENT_DIGEST),
            None,
        ),
        (
            "reloading",
            Some("applied"),
            Some(SOURCE_COMMIT),
            Some(CONTENT_DIGEST),
            None,
        ),
        (
            "resolving_commit",
            Some("no_change"),
            Some(SOURCE_COMMIT),
            Some(CONTENT_DIGEST),
            None,
        ),
        ("fetching", Some("failed"), None, None, Some("fetch_failed")),
        (
            "compiling",
            Some("cancelled"),
            Some(SOURCE_COMMIT),
            None,
            None,
        ),
    ] {
        let resource = source_sync(stage, outcome, source_commit, content_digest, failure_code);
        assert!(
            serde_json::from_value::<SourceSyncResource>(resource).is_ok(),
            "rejected valid {stage}/{outcome:?} lifecycle"
        );
    }
}

#[test]
fn source_sync_wire_rejects_impossible_terminal_shapes() {
    let cases = [
        (
            source_sync(
                "reloading",
                Some("applied"),
                Some(SOURCE_COMMIT),
                None,
                None,
            ),
            "applied result without a content digest",
        ),
        (
            source_sync(
                "compiling",
                Some("applied"),
                Some(SOURCE_COMMIT),
                Some(CONTENT_DIGEST),
                None,
            ),
            "applied result at the compiling stage",
        ),
        (
            source_sync(
                "fetching",
                Some("no_change"),
                Some(SOURCE_COMMIT),
                Some(CONTENT_DIGEST),
                None,
            ),
            "no-change result at the fetching stage",
        ),
        (
            source_sync("fetching", Some("failed"), None, None, None),
            "failed result without a failure code",
        ),
        (
            source_sync("fetching", Some("cancelled"), None, None, Some("internal")),
            "cancelled result with a failure code",
        ),
    ];
    for (resource, case) in cases {
        assert_sync_rejected(resource, case);
    }

    let mut missing_finish = source_sync(
        "reloading",
        Some("applied"),
        Some(SOURCE_COMMIT),
        Some(CONTENT_DIGEST),
        None,
    );
    missing_finish["finished_at"] = Value::Null;
    assert_sync_rejected(missing_finish, "terminal result without a finish time");

    let mut premature_finish = source_sync("fetching", None, None, None, None);
    premature_finish["finished_at"] = json!("2026-09-04T12:00:01Z");
    assert_sync_rejected(premature_finish, "active result with a finish time");
}

#[test]
fn source_sync_wire_rejects_impossible_active_provenance() {
    for (resource, case) in [
        (
            source_sync("queued", None, Some(SOURCE_COMMIT), None, None),
            "queued result with a commit",
        ),
        (
            source_sync("resolving_commit", None, None, Some(CONTENT_DIGEST), None),
            "resolving result with a digest",
        ),
        (
            source_sync("preparing_candidate", None, None, None, None),
            "candidate result without a commit",
        ),
        (
            source_sync(
                "compiling",
                None,
                Some(SOURCE_COMMIT),
                Some(CONTENT_DIGEST),
                None,
            ),
            "compiling result with a digest",
        ),
        (
            source_sync("reloading", None, Some(SOURCE_COMMIT), None, None),
            "reloading result without a digest",
        ),
    ] {
        assert_sync_rejected(resource, case);
    }
}

#[test]
fn source_sync_wire_rejects_invalid_versions_and_timestamp_ordering() {
    let mut zero_version = source_sync("queued", None, None, None, None);
    zero_version["version"] = json!(0);
    assert_sync_rejected(zero_version, "zero operation version");

    let mut version_outside_sqlite = source_sync("queued", None, None, None, None);
    version_outside_sqlite["version"] = json!(i64::MAX as u64 + 1);
    assert_sync_rejected(version_outside_sqlite, "version outside SQLite range");

    let mut backwards_update = source_sync("queued", None, None, None, None);
    backwards_update["updated_at"] = json!("2026-09-04T11:59:59Z");
    assert_sync_rejected(backwards_update, "update before request");

    let mut backwards_finish = source_sync(
        "reloading",
        Some("applied"),
        Some(SOURCE_COMMIT),
        Some(CONTENT_DIGEST),
        None,
    );
    backwards_finish["finished_at"] = json!("2026-09-04T12:00:00Z");
    assert_sync_rejected(backwards_finish, "finish before update");
}

#[test]
fn source_status_decode_rejects_an_impossible_embedded_sync() {
    let status = managed_source_status(
        None,
        None,
        Some(source_sync("queued", None, Some(SOURCE_COMMIT), None, None)),
        None,
    );

    assert!(serde_json::from_value::<SourceStatusResponse>(status).is_err());
}

#[test]
fn source_status_wire_enforces_installation_and_active_operation_shapes() {
    let applied = source_sync(
        "reloading",
        Some("applied"),
        Some(SOURCE_COMMIT),
        Some(CONTENT_DIGEST),
        None,
    );
    assert!(
        serde_json::from_value::<SourceStatusResponse>(managed_source_status(
            Some(SOURCE_COMMIT),
            Some(CONTENT_DIGEST),
            None,
            Some(applied.clone()),
        ))
        .is_ok()
    );
    assert!(
        serde_json::from_value::<SourceStatusResponse>(json!({
            "mode": "external_checkout"
        }))
        .is_ok()
    );

    for (status, case) in [
        (
            managed_source_status(Some(SOURCE_COMMIT), None, None, None),
            "installation without a content digest",
        ),
        (
            managed_source_status(None, Some(CONTENT_DIGEST), None, None),
            "installation without a source commit",
        ),
        (
            managed_source_status(None, None, Some(applied), None),
            "terminal operation reported as active",
        ),
    ] {
        assert!(
            serde_json::from_value::<SourceStatusResponse>(status).is_err(),
            "accepted {case}"
        );
    }
}
