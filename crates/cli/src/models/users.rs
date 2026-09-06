use clap::{Args, Subcommand};
use maincopy_shared::auth::{UserRole, UserStatus};
use uuid::Uuid;

#[derive(Debug, Subcommand)]
pub(crate) enum UserCommand {
    /// Fetch at most 100 accounts, ordered by UUID.
    List {
        /// Continue with the next cursor from a previous page.
        #[arg(long)]
        cursor: Option<Uuid>,
    },
    /// Inspect account state, roles, scopes, and public credential metadata.
    Inspect { user_id: Uuid },
    /// Enable or disable an account at its inspected version.
    Status {
        #[command(flatten)]
        target: UserTarget,
        #[arg(long, value_parser = parse_user_status)]
        status: UserStatus,
    },
    /// Replace the complete role set. Requires a fresh Owner session.
    Roles {
        #[command(flatten)]
        target: UserTarget,
        #[arg(long, required = true, num_args = 1..=3, value_delimiter = ',', value_parser = parse_user_role)]
        roles: Vec<UserRole>,
    },
}

#[derive(Debug, Args)]
pub(crate) struct UserTarget {
    pub(crate) user_id: Uuid,
    /// Current account version from users inspect.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..i64::MAX as u64))]
    pub(crate) expected_version: u64,
    /// Retry identity; generated when omitted. Reuse only for the identical command.
    #[arg(long)]
    pub(crate) idempotency_key: Option<Uuid>,
}

fn parse_user_status(value: &str) -> Result<UserStatus, clap::Error> {
    UserStatus::parse(value).ok_or_else(|| {
        clap::Error::raw(
            clap::error::ErrorKind::InvalidValue,
            "status must be enabled or disabled",
        )
    })
}

fn parse_user_role(value: &str) -> Result<UserRole, clap::Error> {
    UserRole::parse(value).ok_or_else(|| {
        clap::Error::raw(
            clap::error::ErrorKind::InvalidValue,
            "role must be owner, administrator, or publisher",
        )
    })
}
