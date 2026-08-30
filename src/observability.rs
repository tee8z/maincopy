use std::fmt;

use serde::{Deserialize, Serialize, de};
use thiserror::Error;
use tracing::Span;
use uuid::Uuid;

use crate::error::CriticalTaskName;

pub const REQUEST_CORRELATION_FIELD: &str = "request_id";
pub const TASK_CORRELATION_FIELD: &str = "task_id";
pub const TASK_NAME_FIELD: &str = "task_name";

macro_rules! correlation_id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn parse(value: &str) -> Result<Self, CorrelationIdParseError> {
                let parsed = Uuid::parse_str(value).map_err(|_| CorrelationIdParseError)?;
                if parsed.hyphenated().to_string() != value {
                    return Err(CorrelationIdParseError);
                }
                Ok(Self(parsed))
            }

            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}", self.0.hyphenated())
            }
        }

        impl Serialize for $name {
            fn serialize<Serializer>(
                &self,
                serializer: Serializer,
            ) -> Result<Serializer::Ok, Serializer::Error>
            where
                Serializer: serde::Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<Deserializer>(
                deserializer: Deserializer,
            ) -> Result<Self, Deserializer::Error>
            where
                Deserializer: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(&value).map_err(de::Error::custom)
            }
        }
    };
}

correlation_id_type!(RequestCorrelationId);
correlation_id_type!(TaskCorrelationId);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("correlation ID must be a canonical lowercase hyphenated UUID")]
pub struct CorrelationIdParseError;

/// Creates one structured span for a request lifecycle.
pub fn request_span(correlation: RequestCorrelationId) -> Span {
    tracing::info_span!("request", request_id = %correlation)
}

/// Creates one structured span for an application-owned task lifecycle.
pub fn task_span(correlation: TaskCorrelationId, task: CriticalTaskName) -> Span {
    tracing::info_span!("task", task_id = %correlation, task_name = %task)
}

#[cfg(test)]
mod tests {
    use tracing::subscriber::with_default;
    use tracing_subscriber::Registry;

    use super::*;

    #[test]
    fn correlation_ids_have_canonical_stable_wire_values() {
        let value = "2e776d7d-7d5f-4ab7-8c63-434c66a262aa";
        let request = RequestCorrelationId::parse(value).unwrap();
        let task = TaskCorrelationId::parse(value).unwrap();

        assert_eq!(request.to_string(), value);
        assert_eq!(task.to_string(), value);
        assert_eq!(serde_json::to_value(request).unwrap(), value);
        assert_eq!(serde_json::to_value(task).unwrap(), value);
        for invalid in [
            "2E776D7D-7D5F-4AB7-8C63-434C66A262AA",
            "2e776d7d7d5f4ab78c63434c66a262aa",
            "not-a-uuid",
        ] {
            assert!(RequestCorrelationId::parse(invalid).is_err());
        }
    }

    #[test]
    fn spans_lock_request_and_task_correlation_field_names() {
        with_default(Registry::default(), || {
            let request = request_span(RequestCorrelationId::new());
            let request_metadata = request.metadata().unwrap();
            assert_eq!(request_metadata.name(), "request");
            assert!(
                request_metadata
                    .fields()
                    .field(REQUEST_CORRELATION_FIELD)
                    .is_some()
            );

            let task = task_span(TaskCorrelationId::new(), CriticalTaskName::Scheduler);
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
}
