use std::env;

use tracing::{Span, level_filters::LevelFilter};
use uuid::Uuid;

use crate::error::CriticalTaskName;

#[cfg(test)]
const TASK_CORRELATION_FIELD: &str = "task_id";
#[cfg(test)]
const TASK_NAME_FIELD: &str = "task_name";

/// Installs the process-wide structured log subscriber when the embedding
/// process has not already installed one.
pub(crate) fn initialize_logging() {
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(get_log_level())
        .with_writer(std::io::stderr)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

pub(crate) fn get_log_level() -> LevelFilter {
    env::var("RUST_LOG").map_or(LevelFilter::INFO, |value| parse_log_level(&value))
}

fn parse_log_level(value: &str) -> LevelFilter {
    match value.to_ascii_lowercase().as_str() {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "warn" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        _ => LevelFilter::INFO,
    }
}

/// Creates one structured span for an application-owned task lifecycle.
pub(crate) fn task_span(task: CriticalTaskName) -> Span {
    let correlation = Uuid::new_v4();
    tracing::info_span!("task", task_id = %correlation, task_name = %task)
}

#[cfg(test)]
mod tests {
    use tracing::subscriber::with_default;
    use tracing_subscriber::Registry;

    use super::*;

    #[test]
    fn task_span_locks_its_correlation_field_names() {
        with_default(Registry::default(), || {
            let task = task_span(CriticalTaskName::Scheduler);
            let task_metadata = task.metadata().unwrap();
            assert_eq!(task_metadata.name(), "task");
            assert!(
                task_metadata
                    .fields()
                    .field(TASK_CORRELATION_FIELD)
                    .is_some()
            );
            assert!(task_metadata.fields().field(TASK_NAME_FIELD).is_some());
        });
    }

    #[test]
    fn simple_rust_log_levels_are_case_insensitive_and_default_to_info() {
        assert_eq!(parse_log_level("trace"), LevelFilter::TRACE);
        assert_eq!(parse_log_level("DEBUG"), LevelFilter::DEBUG);
        assert_eq!(parse_log_level("Warn"), LevelFilter::WARN);
        assert_eq!(parse_log_level("error"), LevelFilter::ERROR);
        assert_eq!(parse_log_level(""), LevelFilter::INFO);
        assert_eq!(parse_log_level("maincopy=debug"), LevelFilter::INFO);
    }
}
