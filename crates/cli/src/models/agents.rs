use clap::{Args, Subcommand};
use maincopy_shared::auth::AdminScope;
use time::OffsetDateTime;
use uuid::Uuid;

use super::parse_utc_rfc3339;

#[derive(Debug, Subcommand)]
pub(crate) enum AgentCommand {
    /// Fetch at most 100 grants, ordered by UUID.
    List {
        #[arg(long)]
        cursor: Option<Uuid>,
    },
    /// Inspect public key, fingerprint, ownership, scopes, and lifecycle state.
    Inspect { agent_id: Uuid },
    /// Register a public key with selected permissions. Requires fresh authentication.
    Register {
        #[arg(long)]
        owner_user_id: Uuid,
        #[arg(long)]
        public_key: Box<str>,
        #[arg(long)]
        label: Box<str>,
        #[arg(long, required = true, num_args = 1..=12, value_delimiter = ',', value_parser = parse_scope)]
        scopes: Vec<AdminScope>,
        #[arg(long, value_name = "UTC_RFC3339", value_parser = parse_utc_rfc3339)]
        expires_at: Option<OffsetDateTime>,
        /// Retry identity; generated when omitted. Reuse only for the identical command.
        #[arg(long)]
        idempotency_key: Option<Uuid>,
    },
    /// Replace all requested scopes at the inspected grant version.
    Scopes {
        #[command(flatten)]
        target: AgentTarget,
        #[arg(long, required = true, num_args = 1..=12, value_delimiter = ',', value_parser = parse_scope)]
        scopes: Vec<AdminScope>,
    },
    /// Revoke agent access at the inspected grant version.
    Revoke(AgentTarget),
}

#[derive(Debug, Args)]
pub(crate) struct AgentTarget {
    pub(crate) agent_id: Uuid,
    /// Exact grant version from agents inspect.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..i64::MAX as u64))]
    pub(crate) expected_version: u64,
    #[arg(long)]
    pub(crate) idempotency_key: Option<Uuid>,
}

fn parse_scope(value: &str) -> Result<AdminScope, clap::Error> {
    AdminScope::parse(value).ok_or_else(|| {
        clap::Error::raw(
            clap::error::ErrorKind::InvalidValue,
            "scope must be a supported administration scope",
        )
    })
}
