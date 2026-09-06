# System evidence matrix

Use this index to assess [implementation plan §3.1](implementation.md#31-run-the-end-to-end-matrix).
It maps existing assertions to lifecycle cases; it does not certify a candidate test run.
Record the revision, platform, command, date, and retained output before closing an acceptance item.

Process fixtures start `maincopyd` with SQLite and controlled peers.
Router and component fixtures exercise application boundaries without an interactive browser.
Native backup fixtures use real Litestream and rclone, but replace B2 transport and the Maincopy manifest command.
The deployment VM uses the packaged Rust commands. Its passing canonical run is recorded below.

## Lifecycle coverage

| Case | Existing test evidence | Scope |
| --- | --- | --- |
| Startup | [production_identity_policy_refuses_credential_output_and_accepts_offline_nostr_bootstrap](../crates/server/src/startup.rs#L2138); [application_build_serves_public_site_and_protected_admin_backend](../crates/server/src/startup.rs#L2295) | Application fixtures check explicit bootstrap, listeners, and protected administration. |
| Browser login | [protected_router_enforces_host_origin_cookie_csrf_and_logout](../crates/server/src/admin/security.rs#L2329); [nostr_login_consumes_one_signed_challenge_exactly_once](../crates/server/src/admin/security.rs#L2526) | Router fixtures exercise password sessions and signed challenge replay. Signing keys are test fixtures. |
| Human CLI login | [external_human_signing_request_binds_the_exact_origin_and_creates_the_human_wire_proof](../crates/cli/src/client/nostr_login.rs#L373); [cancelled_signing_and_output_failure_return_before_a_proof_is_submitted](../crates/cli/src/startup/nostr_login.rs#L67) | Client and prompt fixtures check proof binding and cancellation. No owner signer participates. |
| Agent API authentication | [agent_proofs_are_exact_replay_protected_scoped_and_restore_the_body](../crates/server/src/admin/security.rs#L2575); [browser_agents_register_replay_replace_scopes_and_revoke_access](../crates/server/tests/api/admin_identity/agent_workflows.rs#L7) | Router proofs and a daemon process exercise scopes, replay, registration, and revocation. |
| Managed Git sync | [real_managed_git_poll_updates_only_the_private_preview_without_a_restart](../crates/server/tests/api/managed_source.rs#L60) | A daemon, Git repository, and constrained SSH fixture update the private candidate while public bytes remain pinned. |
| Preview | [edited_markdown_updates_private_preview_until_explicit_publication_approval](../crates/server/src/startup.rs#L2440); [preview_selection_validates_server_identity_before_creating_the_file](../crates/cli/src/startup.rs#L2780) | Application and CLI fixtures check private preview updates and output identity validation. |
| Immediate release | [admin_publication_route_activates_the_public_site_and_replays_success](../crates/server/src/startup.rs#L2344) | Application HTTP requests activate the public snapshot and replay the accepted result. |
| Scheduled release | [browser_schedule_preserves_reviewed_revision_and_recovers_lost_response](../crates/server/src/domain/publication/ui.rs#L1749); [due_publication_activation_updates_the_snapshot_projection_and_durable_ledger](../crates/server/src/domain/publication/scheduler.rs#L467) | Browser-form and scheduler fixtures preserve the reviewed revision, then activate the due release. |
| Published update | [update_supersedes_one_release_and_startup_selects_only_the_new_digest](../crates/server/src/domain/publication/activation.rs#L4305) | Coordinator and SQLite assertions cover supersession, updated output, replay, and startup selection. |
| Cancellation | [browser_edits_and_cancels_exact_release_versions_with_durable_replay](../crates/server/src/domain/publication/ui.rs#L2084); [browser_cancels_a_blocked_release_without_publishing_it](../crates/server/src/domain/publication/ui.rs#L2018) | Router fixtures check versioned changes, durable replay, and cancellation without publication. |
| Blocked retry | [browser_retries_the_original_blocked_release_and_replays_its_receipt](../crates/server/src/domain/publication/ui.rs#L1946) | The router retries the same release and pinned revision, then replays its receipt. |
| Tips | [browser_profiles_and_tip_selection_preserve_versions_replay_and_restart](../crates/server/tests/api/admin_identity/profile_workflows.rs#L54) | Daemon restarts preserve selection. Stale edits fail; disabling the profile removes recipient eligibility. |
| Metrics | [application_registries_are_isolated_and_expose_only_bounded_metric_labels](../crates/server/src/metrics/mod.rs#L173); [metrics_connections_obey_the_shared_capacity_and_lifetime](../crates/server/src/metrics/server.rs#L237) | Registry and TCP fixtures check label bounds, registry isolation, socket capacity, and expiration. |
| Encrypted backup | [test_native_checkpoint_is_encrypted_offsite_and_replays](../nix/tests/test-operations.py#L287); [test_interrupted_upload_keeps_previous_complete_recovery_and_health_time](../nix/tests/test-operations.py#L301) | Native-tool fixtures check ciphertext, replay, interrupted upload, and preservation of the preceding recovery point. |
| Restore | [an_exported_backup_restores_released_pages_feed_profiles_tips_and_revokes_old_sessions](../crates/server/tests/api/managed_source/restore.rs#L9) | A daemon process restores a portable bundle and compares pages, feed, sitemap, profile, tips, and rejected old sessions. |
| Shutdown | [shutdown_finishes_an_accepted_public_request_before_closing_the_real_writer](../crates/server/src/startup.rs#L3428); [shutdown_drains_producers_before_stopping_the_database_writer](../crates/server/src/startup.rs#L3601) | Application fixtures complete accepted requests and drain producers before writer shutdown. |

## Failure coverage

These are representative failure assertions, not an exhaustive injection record for every stage.

| Boundary | Existing test evidence | Observable assertion |
| --- | --- | --- |
| Startup stages | [host_configuration_failure_prevents_content_discovery](../crates/server/src/startup.rs#L2060); [admin_listener_failure_releases_public_listener_and_database_ownership](../crates/server/src/startup.rs#L3313) | Selected configuration and listener failures stop startup and release owned resources. |
| Writer boundary | [begin_publication_recovers_at_both_crash_boundaries](../crates/server/src/database/writer.rs#L1765); [bounded_queue_rejects_full_then_drains_accepted_commands_on_shutdown](../crates/server/src/database/writer.rs#L2049) | Child-process crashes and queue saturation exercise recovery and accepted-write draining. |
| Activation boundary | [activation_conflict_fails_closed_and_retry_resumes_the_durable_intent](../crates/server/src/domain/publication/activation.rs#L3979) | A conflicting snapshot blocks activation; retry resumes the durable intent. |
| Git phases | [managed_source_engine_applies_changes_handles_no_change_and_preserves_last_good](../crates/server/src/git_sync.rs#L2608); [managed_git_wall_time_covers_output_held_open_by_descendants](../crates/server/tests/api/managed_source.rs#L394) | Invalid content preserves the installation. Descendant-held output remains deadline-bound. |
| Renderer phases | [shared_renderer_handles_concurrent_protocol_jobs](../crates/diagram-renderer/tests/render_protocol.rs#L7); [mermaid_timeout_rejects_the_article_with_the_authored_code_block_location](../crates/server/src/render/markdown.rs#L1843) | Concurrent native jobs, malformed input, resource limits, and an injected timeout exercise rejection boundaries. |
| Gateway routes | [deployment-vm testScript](../nix/tests/deployment-vm.nix) | The passing VM checks route isolation, host mismatch, origin rejection, spoofed identity headers, and credential isolation across repeated backup cycles. |
| Restore gates | [checkpoint_publication_refuses_missing_or_corrupt_required_candidates](../crates/server/tests/api/managed_source/restore.rs#L197); [acceptance_rejects_changed_database_artifacts_schema_binary_and_sidecars](../crates/server/src/restore.rs#L1059) | Rust fixtures reject missing content and altered acceptance inputs. The checkpoint publication fixture uses opaque LTX bytes. |
| Backup freshness and retention | [test_stopped_replica_cannot_refresh_an_old_complete_checkpoint](../nix/tests/test-operations.py#L355); [test_interrupted_local_retention_is_cleaned_under_the_backup_lock](../nix/tests/test-operations.py#L376) | Native-tool fixtures reject stopped replication and reclaim interrupted local retention. |
| Critical metrics failure | [metrics_collector_storage_failure_marks_unready_and_drains_the_writer](../crates/server/src/startup.rs#L3702) | A collector failure makes readiness unavailable and drains the writer. |

## Recorded measurements

The [backup runbook](backup-restore.md#recovery-drill-evidence) records these small-fixture measurements:

| Fixture | Recorded result | Limit |
| --- | --- | --- |
| Portable restore, 2026-09-06 | Zero acknowledged fixture changes lost; 1.171 seconds from acceptance through listener startup | Excludes download and decryption |
| Same portable test | 5.07 seconds for the development test binary | Covers the portable bundle, not an off-site checkpoint drill |
| Checkpoint content verification | 245 milliseconds for schema, identity, compilation, and hashing with intact inputs | Excludes native replay, encryption, and upload |

These observations are not production latency or recovery guarantees.

## Canonical validation record

The integrated operations tree passed these checks on 2026-09-06, before its signed commit:

| Check | Result |
| --- | --- |
| Concurrent Rust tests | 994 passed; one existing ignored test; 16 test threads |
| Rust formatting and Clippy | Passed with warnings denied |
| Manual CRAP check | Zero violations; maximum 19.662785; 93.67% line coverage (66,512/71,010) |
| CRAP measurement scope | 3,756 measured functions; 379 functions had no instrumented lines |
| Canonical `nix flake check` and `nix build` | Passed on `x86_64-linux` |
| Native Litestream/rclone fixture | 17 tests passed |
| Packaged deployment VM | Passed; test script completed in 65.40 seconds |

The VM verifies initial owner setup, gateway boundaries, repeated credential isolation,
encrypted checkpoint publication, interrupted upload, and recovery into the actual service state directory.
After recovery, it verifies marker consumption before replication and publishes another checkpoint.
It also checks unsafe database permissions and subsequent normal restart.
Only B2 transport is replaced; cryptography, Litestream, Rust verification, and service isolation remain real.

Restore rejection tests also cover dangling lifecycle markers and SQLite sidecars.
They assert unchanged database bytes, acceptance records, and rejected filesystem entries.

These results establish fixture behavior. They do not complete actual B2, owner-signer,
other-platform, or production performance acceptance.

## Release-readiness validation record

The release-readiness source passed the canonical Linux checks on 2026-09-06.
The retained preparation record identifies the tested tree, package, and committed archive.

| Check | Result |
| --- | --- |
| Canonical `nix flake check` and `nix build` | Passed from a clean source archive on `x86_64-linux`; other architectures were not executed. |
| Concurrent instrumented Rust suite | 994 passed; one existing ignored test; 16 test threads. |
| Manual CRAP check | Zero violations; maximum 19.662785; 93.69% line coverage (66,744/71,242). |
| CRAP measurement scope | 3,761 measured functions; 379 functions had no instrumented lines. |
| Nix Rust suite | 994 tests passed; one existing ignored test. The package build also passed its documentation test. |
| Rust formatting and Clippy | Passed with warnings denied. |
| Packaged deployment VM | Passed; test script completed in 62.82 seconds. |

The VM now waits for a confirmed native replica cutoff before requesting each checkpoint.
An active Litestream wrapper alone does not establish replication readiness.
See the [deployment procedure](deployment.md#provision-encrypted-b2-backups) for the same operator check.

An earlier instrumented run hit the fixture's 10-second login deadline during overlapping builds.
The unchanged 16-thread rerun passed after those builds finished.
No deadline or authentication behavior was changed for that rerun.

## Polling fixture coordination exception

Review approves a narrow exception to the test-only visibility rule in
[quality.md](quality.md#modules-and-imports).
`SourcePollArm` and `observe_poll_arms` exist only under `cfg(test)`.
They provide a bounded handoff for the cross-module Git polling fixture.
Production API and polling behavior remain unchanged.

The fixture holds re-arming after the durable manual result, then advances virtual time.
It releases the actor and waits for the constructed timer's actual deadline before advancing again.
This forces the interleaving that two scheduler yields could not exclude.
The focused test passed once, followed by 16 successful repetitions across four
concurrent processes restricted to two CPUs. Clippy passed with warnings denied.

The prior remote run for `443a2c8` failed at this polling fixture's 10-second wait.
That remote failure does not invalidate the recorded local results, but it required this repair.
Hosted validation of the repaired source remains separate evidence.

[Hosted CI for `459573a`](https://github.com/tee8z/maincopy/actions/runs/34059358517)
passed on 2026-09-06. Both the Linux Nix job and Windows client job completed successfully.

A later local Nix run failed the metrics fixture's final listener rebind with
`AddrInUse`. Its non-listening reservation did not prevent competing explicit binds
to addresses retained by other fixtures. The test now uses a dedicated loopback
address and retains its negative control, drain checks, and fresh final bind.

A targeted socket control reproduced this collision class. It did not identify
the competing socket from the earlier Nix failure. Both the baseline and revised
fixtures passed 480 executions across four concurrent processes.

## Release automation validation record

The release-automation source passed the final local gates on 2026-09-06.
The retained record identifies the frozen source archive and its Nix package.
Ongoing email implementation was excluded from this release-automation batch.

| Check | Result |
| --- | --- |
| Canonical `nix flake check` and `nix build` | Passed from a clean source archive on `x86_64-linux`. ARM64 was not executed. |
| Concurrent instrumented Rust suite | 994 passed; one existing ignored test; 16 test threads. |
| Manual CRAP check | Zero violations; maximum 19.662785; 93.69% line coverage (66,803/71,300). |
| CRAP measurement scope | 3,763 measured functions; 379 functions had no instrumented lines. |
| Nix Rust suite | 994 passed; one existing ignored test. The package build also passed its documentation test. |
| Rust formatting and Clippy | Passed with warnings denied. |
| Packaged deployment VM | Passed; test script completed in 68.47 seconds. |
| Release helper boundaries | 13 tests passed with the flake-locked Cargo toolchain, real GPG signatures, and a loopback registry. |
| Workflow and helper lint | Actionlint and Python Ruff checks passed. |

Both Nix Rust suites passed the repaired polling and metrics lifecycle fixtures.
The release helper tests exercised checksum rejection, dependency ordering, interrupted
publication, matching retries, and credential-provider configuration overrides.
They created no public tags, registry uploads, or GitHub Releases.

These results do not complete ARM64 runner, protected release environment, signer,
publisher account, provider integration, or deployed-system acceptance.
The CRAP report measures Rust functions; it does not score the Python release helpers.

## Documentation and packaging rehearsal

The release-readiness audit on 2026-09-06 used a clean, private operator fixture
and the packaged operations build. It completed these checks:

| Check | Result and scope |
| --- | --- |
| Executed operator checks | 24 passed: frontmatter, bootstrap, startup, metrics, route isolation, portable export/recovery, source setup, and local encryption/decryption. |
| Documented CLI options | 63 packaged `--help` invocations passed with fixture substitutions. Execution and required-argument completeness need separate checks. |
| Configuration examples | Seven TOML blocks parsed; the README article passed the packaged content validator. |
| Documentation links | Local Markdown file, heading, and source-line targets resolved across the repository. |
| Documented shell syntax | 76 shell and console blocks parsed with fixture substitutions; ten are release Bash blocks. |
| Crate contents | All five packages passed clean workspace publish dry runs without upload. Each contains its README and exact MIT license bytes; normalized dependencies use registry versions. |
| Packaged OpenAPI and authentication | 152 checks passed across 38 operations, four authentication schemes, and 298 local schema references. Login, cookie protection, CSRF, logout, and listener isolation passed. |
| Concurrent listener lifecycle | Nine focused tests passed, followed by 32 repetitions across four concurrent processes: 297 successful test executions. |
| Checksum retry | Initial generation, identical retry, changed-artifact rejection, and deliberate manifest regeneration passed. |

Rendering constraints now live in [content rendering](content-rendering.md).
The [deployment runbook](deployment.md#service-identities-and-filesystem-boundaries)
retains backup writer, process-isolation, and secret-buffer exceptions.
Historical decision files were removed.

The fixture encryption checks used generated local keys. They did not contact B2.
The CLI grammar checks did not use the owner's signer or operating-system keychain.
The [release runbook](release.md) distinguishes preparation from publication.
The packaged authentication fixture used fresh local credentials and loopback HTTP.
It does not complete browser, TLS, or owner-signer acceptance.

## Pending acceptance

- Run one representative managed Git site through the full browser, human CLI, and agent API lifecycle. Preserve the operation identities and results.
- Verify sign-in and cancellation with the owner's browser signer and human CLI signer. Record signer versions, operating systems, and protected session storage.
- Complete deployed-host gateway, service-isolation, backup, restore, and first-start checks from the [deployment runbook](deployment.md). Keep production evidence separate from the passing VM fixture.
- Publish to the owner's actual B2 account and restore a complete encrypted checkpoint with the independent recovery key copy.
- Verify recovery from retained local ciphertext. Record the selected checkpoint, cutoff, package, elapsed recovery time, and off-site object identities.
- Complete the remaining stage-by-stage failure injections. Confirm public snapshot continuity wherever required by the design.
- Measure representative request and compilation latency, queue depth, WAL size, checkpoint age, runtime use, and shutdown duration.
- Retain final Rust, CRAP, Nix, and runbook execution results for the exact candidate. This index does not replace those results.

The selected distribution uses all five crates.io packages, GitHub release
artifacts, and the tagged GitHub flake. The first version, publisher configuration,
and actual publication remain pending.
