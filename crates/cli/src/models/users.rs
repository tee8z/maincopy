use crate::nip98::inspect_public_key;
use clap::{Args, Subcommand};
use maincopy_shared::auth::{HumanLoginProvider, UserRole, UserStatus};
use uuid::Uuid;

#[derive(Debug, Subcommand)]
pub(crate) enum UserCommand {
    /// Create an account with initial login credentials. Requires fresh authentication.
    Create(CreateUserArguments),
    /// Add, replace, or remove account login credentials.
    Credentials {
        user_id: Uuid,
        #[command(subcommand)]
        command: UserCredentialCommand,
    },
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

#[derive(Debug, Args)]
#[command(subcommand_precedence_over_arg = true)]
pub(crate) struct CreateUserArguments {
    #[arg(long, default_value = "enabled", value_parser = parse_user_status)]
    pub(crate) status: UserStatus,
    #[arg(long, required = true, num_args = 1..=3, value_delimiter = ',', value_parser = parse_user_role)]
    pub(crate) roles: Vec<UserRole>,
    #[arg(long)]
    pub(crate) idempotency_key: Option<Uuid>,
    #[command(subcommand)]
    pub(crate) credentials: InitialCredentials,
}

#[derive(Debug, Subcommand)]
pub(crate) enum InitialCredentials {
    Password {
        #[arg(long, value_parser = parse_username)]
        username: Box<str>,
    },
    Nostr {
        #[arg(long, value_parser = parse_public_key)]
        public_key: Box<str>,
    },
    Both {
        #[arg(long, value_parser = parse_username)]
        username: Box<str>,
        #[arg(long, value_parser = parse_public_key)]
        public_key: Box<str>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum LoginCredentialArguments {
    Password {
        #[arg(long, value_parser = parse_username)]
        username: Box<str>,
    },
    Nostr {
        #[arg(long, value_parser = parse_public_key)]
        public_key: Box<str>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum UserCredentialCommand {
    /// Add a provider that this account does not yet have.
    Add {
        #[arg(long)]
        idempotency_key: Option<Uuid>,
        #[command(subcommand)]
        credential: LoginCredentialArguments,
    },
    /// Replace a credential at its inspected credential version.
    Replace {
        #[command(flatten)]
        target: CredentialTarget,
        #[command(subcommand)]
        credential: LoginCredentialArguments,
    },
    /// Remove a credential at its inspected credential version.
    Remove {
        #[command(flatten)]
        target: CredentialTarget,
        #[arg(long, value_parser = parse_provider)]
        provider: HumanLoginProvider,
    },
}

#[derive(Debug, Args)]
pub(crate) struct CredentialTarget {
    /// Credential version from users inspect, separate from the account version.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..i64::MAX as u64))]
    pub(crate) expected_version: u64,
    #[arg(long)]
    pub(crate) idempotency_key: Option<Uuid>,
}

fn parse_username(value: &str) -> Result<Box<str>, &'static str> {
    let bytes = value.as_bytes();
    if !(1..=64).contains(&bytes.len())
        || !bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        || !bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_.".contains(byte))
    {
        return Err(
            "username must be 1–64 lowercase ASCII letters, digits, '-', '_', or '.', beginning and ending with a letter or digit",
        );
    }
    Ok(value.into())
}

fn parse_public_key(value: &str) -> Result<Box<str>, &'static str> {
    inspect_public_key(value)
        .map(|identity| identity.public_key)
        .map_err(|_| "public key must be a valid x-only secp256k1 key in 64 lowercase hexadecimal characters")
}

fn parse_provider(value: &str) -> Result<HumanLoginProvider, &'static str> {
    HumanLoginProvider::parse(value).ok_or("provider must be password or nostr")
}
