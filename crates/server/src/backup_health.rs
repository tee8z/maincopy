//! A bounded read of an operator-owned replica status report.

use crate::config::BackupStatusConfigurationView;
use serde::Deserialize;
use std::{fs::File, io::Read as _, path::PathBuf, sync::Arc, time::Duration};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::Semaphore;

const MAX_REPORT_BYTES: u64 = 4 * 1024;
const READ_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub(crate) struct BackupHealth {
    source: Option<StatusSource>,
    admission: Arc<Semaphore>,
}

#[derive(Clone)]
struct StatusSource {
    path: PathBuf,
    stale_after: Duration,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BackupHealthState {
    Healthy,
    Degraded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BackupHealthSnapshot {
    pub(crate) state: BackupHealthState,
    pub(crate) last_success_at: Option<OffsetDateTime>,
}

impl BackupHealthSnapshot {
    const DEGRADED: Self = Self {
        state: BackupHealthState::Degraded,
        last_success_at: None,
    };
}

#[derive(Deserialize)]
enum ReportFormat {
    #[serde(rename = "maincopy-backup-status-v1")]
    V1,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusReport {
    format: ReportFormat,
    state: BackupHealthState,
    #[serde(with = "time::serde::rfc3339::option")]
    last_success_at: Option<OffsetDateTime>,
}

impl StatusReport {
    fn into_snapshot(
        self,
        now: OffsetDateTime,
        stale_after: Duration,
    ) -> Result<BackupHealthSnapshot, BackupReportError> {
        match self.format {
            ReportFormat::V1 => {}
        }
        let Some(last_success_at) = self.last_success_at else {
            return Ok(BackupHealthSnapshot::DEGRADED);
        };
        if last_success_at.offset() != UtcOffset::UTC || last_success_at > now {
            return Err(BackupReportError::Invalid);
        }
        let age: Duration = (now - last_success_at)
            .try_into()
            .map_err(|_| BackupReportError::Invalid)?;
        let state = if self.state == BackupHealthState::Healthy && age <= stale_after {
            BackupHealthState::Healthy
        } else {
            BackupHealthState::Degraded
        };
        Ok(BackupHealthSnapshot {
            state,
            last_success_at: Some(last_success_at),
        })
    }
}

impl BackupHealth {
    pub(crate) fn new(configuration: Option<BackupStatusConfigurationView<'_>>) -> Self {
        Self {
            source: configuration.map(|configuration| StatusSource {
                path: configuration.status_file.to_path_buf(),
                stale_after: configuration.stale_after,
            }),
            admission: Arc::new(Semaphore::new(1)),
        }
    }

    pub(crate) async fn snapshot(&self) -> BackupHealthSnapshot {
        let Some(source) = self.source.clone() else {
            return BackupHealthSnapshot::DEGRADED;
        };
        let Ok(admission) = Arc::clone(&self.admission).try_acquire_owned() else {
            return BackupHealthSnapshot::DEGRADED;
        };
        let read = tokio::task::spawn_blocking(move || {
            let _admission = admission;
            inspect_report(&source, OffsetDateTime::now_utc())
        });
        match tokio::time::timeout(READ_TIMEOUT, read).await {
            Ok(Ok(Ok(snapshot))) => snapshot,
            _ => BackupHealthSnapshot::DEGRADED,
        }
    }
}

#[derive(Debug, Error)]
enum BackupReportError {
    #[error("the backup report could not be read")]
    Read(#[from] std::io::Error),
    #[error("the backup report does not match the bounded status contract")]
    Invalid,
    #[error("the backup report is invalid JSON")]
    Json(#[from] serde_json::Error),
}

#[cfg(unix)]
fn open_report(path: &std::path::Path) -> Result<File, std::io::Error> {
    use rustix::fs::{Mode, OFlags, open};
    // A substituted symlink or pipe must neither escape the path nor block shutdown.
    Ok(File::from(open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}
#[cfg(not(unix))]
fn open_report(path: &std::path::Path) -> Result<File, std::io::Error> {
    File::open(path)
}

fn inspect_report(
    source: &StatusSource,
    now: OffsetDateTime,
) -> Result<BackupHealthSnapshot, BackupReportError> {
    let metadata = std::fs::symlink_metadata(&source.path)?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_REPORT_BYTES {
        return Err(BackupReportError::Invalid);
    }
    let file = open_report(&source.path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.len() > MAX_REPORT_BYTES {
        return Err(BackupReportError::Invalid);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.dev() != opened.dev()
            || metadata.ino() != opened.ino()
            || opened.nlink() != 1
            || opened.uid() != rustix::process::geteuid().as_raw()
            || opened.mode() & 0o077 != 0
        {
            return Err(BackupReportError::Invalid);
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_REPORT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REPORT_BYTES {
        return Err(BackupReportError::Invalid);
    }
    let report: StatusReport = serde_json::from_slice(&bytes)?;
    report.into_snapshot(now, source.stale_after)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn missing_backup_configuration_is_degraded_without_disrupting_the_caller() {
        assert_eq!(
            BackupHealth::new(None).snapshot().await,
            BackupHealthSnapshot::DEGRADED
        );
    }
    #[test]
    fn replica_health_requires_a_recent_successful_bounded_report() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("status.json");
        let source = StatusSource {
            path: path.clone(),
            stale_after: Duration::from_secs(300),
        };
        let now = OffsetDateTime::from_unix_timestamp(1000).unwrap();
        let valid = r#"{"format":"maincopy-backup-status-v1","state":"healthy","last_success_at":"1970-01-01T00:15:00Z"}"#;
        std::fs::write(&path, valid).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(
            inspect_report(&source, now).unwrap().state,
            BackupHealthState::Healthy
        );
        assert_eq!(
            inspect_report(&source, now + time::Duration::seconds(300))
                .unwrap()
                .state,
            BackupHealthState::Degraded
        );
        std::fs::write(&path, valid.replace("healthy", "degraded")).unwrap();
        assert_eq!(
            inspect_report(&source, now).unwrap().state,
            BackupHealthState::Degraded
        );
        for invalid in [
            valid.replace("00:15:00", "00:20:00"),
            valid.replace("-v1", "-v2"),
            "x".repeat(4097),
            "{}".to_owned(),
        ] {
            std::fs::write(&path, invalid).unwrap();
            assert!(inspect_report(&source, now).is_err());
        }
        std::fs::remove_file(&path).unwrap();
        assert!(inspect_report(&source, now).is_err());
    }
}
