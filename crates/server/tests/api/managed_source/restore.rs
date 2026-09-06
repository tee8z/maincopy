use super::*;
use maincopy_shared::profile_api::{
    ACTIVE_TIP_RECIPIENT_PATH, ActiveTipRecipientResponse, CURRENT_USER_PROFILE_PATH,
    PutActiveTipRecipientRequest, UpdateUserProfileRequest, UserProfileResponse,
};
use std::os::unix::fs::OpenOptionsExt as _;

#[tokio::test]
async fn an_exported_backup_restores_released_pages_feed_profiles_tips_and_revokes_old_sessions() {
    let fixture = ManagedGitFixture::start().await;
    let post_path = fixture.work.join("site/posts/managed.md");
    let post = fs::read_to_string(&post_path)
        .unwrap()
        .replace("draft = false", "draft = false\ntips = true");
    fs::write(post_path, post).unwrap();
    commit(&fixture.work, "enable tips for recovery evidence");
    run_git(
        &fixture.work,
        [OsStr::new("push"), OsStr::new("origin"), OsStr::new("main")],
    );
    let synchronization = fixture
        .admin_json(Method::POST, SOURCE_SYNCS_PATH, &serde_json::json!({}))
        .await;
    assert_eq!(synchronization.status(), StatusCode::ACCEPTED);
    let synchronization: BeginSourceSyncResponse = response_json(synchronization).await;
    assert_eq!(
        fixture
            .wait_for_terminal_source_sync(synchronization.sync.source_sync_id)
            .await
            .outcome,
        Some(SourceSyncOutcome::Applied)
    );
    let profile = fixture
        .admin_json(
            Method::PUT,
            CURRENT_USER_PROFILE_PATH,
            &UpdateUserProfileRequest {
                display_name: Some("Recovery author".parse().unwrap()),
                lightning_address: Some("tips@example.test".parse().unwrap()),
                tips_enabled: true,
                expected_version: None,
            },
        )
        .await;
    assert_eq!(profile.status(), StatusCode::CREATED);
    let profile: UserProfileResponse = response_json(profile).await;
    let recipient: ActiveTipRecipientResponse =
        response_json(fixture.admin_get(ACTIVE_TIP_RECIPIENT_PATH).await).await;
    let selected = fixture
        .admin_json(
            Method::PUT,
            ACTIVE_TIP_RECIPIENT_PATH,
            &PutActiveTipRecipientRequest {
                user_id: Some(profile.user_id),
                expected_version: recipient.version,
            },
        )
        .await;
    assert_eq!(selected.status(), StatusCode::OK);
    let preview = fixture.admin_get(&preview_path()).await;
    let published = fixture
        .admin_json(
            Method::POST,
            PUBLICATIONS_PATH,
            &PublishNowRequest {
                post_id: Uuid::parse_str(POST_ID).unwrap(),
                preview_digest: preview_digest(&preview),
                expected_revision: None,
                scheduled_for: None,
            },
        )
        .await;
    assert_eq!(published.status(), StatusCode::OK);
    let mut expected = Vec::new();
    for path in ["/posts/managed", "/feed.xml", "/sitemap.xml"] {
        let response = fixture.public_get(path).await;
        assert_eq!(response.status(), StatusCode::OK);
        expected.push((
            path,
            response.headers()[ETAG].clone(),
            response_text(response).await,
        ));
    }
    assert!(expected[0].2.contains("tips@example.test"));
    let ManagedGitFixture {
        daemon,
        root,
        client,
        session: previous_session,
        ..
    } = fixture;
    daemon.stop();
    let snapshot = root.path().join("state/maincopy.db");
    let bundle = root.path().join("backup.tar");
    let output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&bundle)
        .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args([
            "--config",
            "maincopy.toml",
            "export-backup",
            "--database-file",
        ])
        .arg(&snapshot)
        .current_dir(root.path())
        .stdout(output)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert_process_success("export paired backup", CapturedChild::new(child));
    let recovered = root.path().join("decrypted");
    fs::create_dir(&recovered).unwrap();
    fs::set_permissions(&recovered, fs::Permissions::from_mode(0o700)).unwrap();
    tar::Archive::new(fs::File::open(&bundle).unwrap())
        .unpack(&recovered)
        .unwrap();
    let config = fs::read_to_string(root.path().join("maincopy.toml"))
        .unwrap()
        .replace("state_root = \"state\"", "state_root = \"restored-state\"")
        .replace(
            "mirror_root = \"state/source-mirror\"",
            "mirror_root = \"restored-state/source-mirror\"",
        );
    fs::write(
        root.path().join("restored.toml"),
        format!("{config}\n[database]\npath = \"restored-state/database/maincopy.db\"\n"),
    )
    .unwrap();
    let started = Instant::now();
    let child = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args(["--config", "restored.toml", "restore", "--database-file"])
        .arg(recovered.join("database.sqlite3"))
        .arg("--artifact-root")
        .arg(recovered.join("content-candidates"))
        .arg("--manifest-file")
        .arg(recovered.join("manifest.json"))
        .current_dir(root.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert_process_success("accept restored backup", CapturedChild::new(child));
    let database = root.path().join("restored-state/database/maincopy.db");
    assert!(PathBuf::from(format!("{}.restore.json", database.display())).exists());
    let mut command = Command::new(env!("CARGO_BIN_EXE_maincopyd"));
    command
        .args(["--config", "restored.toml"])
        .current_dir(root.path())
        .env("MAINCOPY_SSH_EXECUTABLE", root.path().join("fixture-ssh"));
    let (daemon, addresses) = Daemon::start(command);
    let rto = started.elapsed();
    for (path, etag, html) in expected {
        let response = client
            .get(format!("http://{}{path}", addresses.public))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[ETAG], etag);
        assert_eq!(response_text(response).await, html);
    }
    let admin_url = format!("http://{}", addresses.admin);
    let old = client
        .get(format!("{admin_url}{CURRENT_USER_PROFILE_PATH}"))
        .header(HOST, ADMIN_AUTHORITY)
        .header(ORIGIN, ADMIN_ORIGIN)
        .header(COOKIE, previous_session.cookie_header.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(old.status(), StatusCode::UNAUTHORIZED);
    let fresh = password_login(&client, &admin_url).await;
    let restored_profile = client
        .get(format!("{admin_url}{CURRENT_USER_PROFILE_PATH}"))
        .header(HOST, ADMIN_AUTHORITY)
        .header(ORIGIN, ADMIN_ORIGIN)
        .header(COOKIE, fresh.cookie_header.as_str())
        .send()
        .await
        .unwrap();
    let restored_profile: UserProfileResponse = response_json(restored_profile).await;
    assert_eq!(restored_profile, profile);
    assert!(!PathBuf::from(format!("{}.restore.json", database.display())).exists());
    assert!(PathBuf::from(format!("{}.restore-consumed", database.display())).exists());
    println!(
        "restore fixture drill: RPO=0 acknowledged fixture changes; RTO={}ms",
        rto.as_millis()
    );
    daemon.stop();
}

#[tokio::test]
async fn checkpoint_publication_refuses_missing_or_corrupt_required_candidates() {
    let fixture = ManagedGitFixture::start().await;
    let ManagedGitFixture { daemon, root, .. } = fixture;
    daemon.stop();
    let capture = root.path().join("checkpoint");
    let artifacts = capture.join("content-candidates");
    fs::create_dir_all(capture.join("ltx/9")).unwrap();
    fs::create_dir(&artifacts).unwrap();
    for path in [
        &capture,
        &artifacts,
        &capture.join("ltx"),
        &capture.join("ltx/9"),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let name = "0000000000000001-0000000000000001.ltx";
    fs::write(capture.join("ltx/9").join(name), b"LTXbytes").unwrap();
    // Native replay is the host's boundary. This fixture exercises the publication
    // command with a real Maincopy database and opaque pinned-plan bytes.
    let plan = serde_json::json!({"source":"file:/private/capture","target_path":"/private/replayed.db","replica":"file","min_txid":"0000000000000001","max_txid":"0000000000000001","files":[{"level":9,"name":name,"min_txid":"0000000000000001","max_txid":"0000000000000001","size":8,"timestamp":"2026-09-06T12:00:00Z"}]});
    let plan_file = capture.join("plan.json");
    fs::write(&plan_file, serde_json::to_vec(&plan).unwrap()).unwrap();
    fs::set_permissions(&plan_file, fs::Permissions::from_mode(0o600)).unwrap();
    let output = capture.join("checkpoint.json");
    let launch = || {
        let child = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
            .args([
                "--config",
                "maincopy.toml",
                "checkpoint-manifest",
                "--database-file",
            ])
            .arg(root.path().join("state/maincopy.db"))
            .arg("--plan-file")
            .arg(&plan_file)
            .arg("--ltx-root")
            .arg(capture.join("ltx"))
            .arg("--artifact-root")
            .arg(&artifacts)
            .arg("--output")
            .arg(&output)
            .current_dir(root.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        CapturedChild::new(child)
    };
    let expect_refused = |process: CapturedChild| {
        let (completion, _, stderr) = process.wait(COMMAND_LIMIT);
        assert!(
            !completion.timed_out,
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(
            !completion.status.unwrap().success(),
            "missing or corrupt required archive must prevent publication"
        );
        assert!(!output.exists());
    };
    expect_refused(launch());
    for artifact in fs::read_dir(root.path().join("state/content-candidates")).unwrap() {
        let artifact = artifact.unwrap();
        fs::copy(artifact.path(), artifacts.join(artifact.file_name())).unwrap();
    }
    let changed = fs::read_dir(&artifacts)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let original = fs::read(&changed).unwrap();
    fs::write(&changed, b"corrupt archive").unwrap();
    expect_refused(launch());
    fs::write(&changed, original).unwrap();
    let started = Instant::now();
    assert_process_success("publish recoverable checkpoint manifest", launch());
    assert!(output.exists());

    let verification_time = started.elapsed();
    let wrong_database = capture.join("different-valid.db");
    fs::copy(root.path().join("state/maincopy.db"), &wrong_database).unwrap();
    {
        use sqlx::{ConnectOptions as _, Connection as _};
        let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&wrong_database)
            .connect()
            .await
            .unwrap();
        sqlx::query("UPDATE instance_identity SET version = version + 1")
            .execute(&mut connection)
            .await
            .unwrap();
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        assert_eq!(integrity, "ok");
        connection.close().await.unwrap();
    }
    let config = fs::read_to_string(root.path().join("maincopy.toml"))
        .unwrap()
        .replace("state_root = \"state\"", "state_root = \"rejected-state\"")
        .replace(
            "mirror_root = \"state/source-mirror\"",
            "mirror_root = \"rejected-state/source-mirror\"",
        );
    fs::write(root.path().join("wrong-restore.toml"), config).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_maincopyd"))
        .args([
            "--config",
            "wrong-restore.toml",
            "restore-replica",
            "--database-file",
        ])
        .arg(&wrong_database)
        .arg("--artifact-root")
        .arg(&artifacts)
        .arg("--manifest-file")
        .arg(&output)
        .arg("--ltx-root")
        .arg(capture.join("ltx"))
        .current_dir(root.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (completion, _, stderr) = CapturedChild::new(child).wait(COMMAND_LIMIT);
    assert!(
        !completion.timed_out,
        "{}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(!completion.status.unwrap().success());
    assert!(
        !root.path().join("rejected-state").exists(),
        "a different valid database must be rejected before staging"
    );
    println!(
        "checkpoint manifest fixture: schema, identities, retained compilation and hashing={}ms; native replay and upload excluded",
        verification_time.as_millis()
    );
}
