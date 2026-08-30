use std::{net::SocketAddr, path::PathBuf};

use clap::{Args, Parser, Subcommand};

use crate::config::{DEFAULT_HOST_CONFIGURATION_FILE, HostConfigurationOverrides};

#[derive(Debug, Parser)]
#[command(
    name = "maincopy",
    version,
    about = "One canonical copy. Every channel."
)]
pub(crate) struct ProcessArguments {
    #[command(subcommand)]
    pub(crate) command: ProcessCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ProcessCommand {
    /// Run the public site, admin API, scheduler, and workers.
    Serve(Box<ServeArguments>),

    /// Send an operation to the running server's private admin API.
    Admin(AdminArguments),
}

#[derive(Debug, Args)]
pub(crate) struct ServeArguments {
    /// Read host configuration from this file.
    #[arg(long, value_name = "PATH", default_value = DEFAULT_HOST_CONFIGURATION_FILE)]
    config: PathBuf,

    /// Override the Git-owned publication content root.
    #[arg(long, value_name = "PATH")]
    content_root: Option<PathBuf>,

    /// Override the local persistent state root.
    #[arg(long, value_name = "PATH")]
    state_root: Option<PathBuf>,

    /// Override the local ephemeral runtime root.
    #[arg(long, value_name = "PATH")]
    runtime_root: Option<PathBuf>,

    /// Override the live SQLite database path.
    #[arg(long, value_name = "PATH")]
    database_path: Option<PathBuf>,

    /// Override the public listener address.
    #[arg(long, value_name = "ADDRESS")]
    public_bind: Option<SocketAddr>,

    /// Override the private admin Unix-socket path.
    #[arg(long, value_name = "PATH")]
    admin_socket: Option<PathBuf>,

    /// Override the SQLite busy timeout in milliseconds.
    #[arg(long, value_name = "MILLISECONDS")]
    database_busy_timeout_ms: Option<u64>,

    /// Override the bounded database writer queue capacity.
    #[arg(long, value_name = "COUNT")]
    database_writer_queue_capacity: Option<u64>,

    /// Override the query-only database read pool size.
    #[arg(long, value_name = "COUNT")]
    database_read_pool_size: Option<u64>,

    /// Override the publication TOML file limit in bytes.
    #[arg(long, value_name = "BYTES")]
    content_publication_file_bytes: Option<u64>,

    /// Override each post file limit in bytes.
    #[arg(long, value_name = "BYTES")]
    content_post_file_bytes: Option<u64>,

    /// Override each content asset file limit in bytes.
    #[arg(long, value_name = "BYTES")]
    content_asset_file_bytes: Option<u64>,

    /// Override the complete managed content-tree limit in bytes.
    #[arg(long, value_name = "BYTES")]
    content_total_tree_bytes: Option<u64>,

    /// Override the maximum managed content entry count.
    #[arg(long, value_name = "COUNT")]
    content_entries: Option<u64>,

    /// Override the maximum managed content path depth.
    #[arg(long, value_name = "COUNT")]
    content_depth: Option<u64>,

    /// Override the maximum portable logical path length in bytes.
    #[arg(long, value_name = "BYTES")]
    content_path_bytes: Option<u64>,
}

impl ServeArguments {
    pub(crate) fn into_configuration(self) -> (PathBuf, HostConfigurationOverrides) {
        let mut overrides = HostConfigurationOverrides::default();
        if let Some(value) = self.content_root {
            overrides = overrides.with_content_root(value);
        }
        if let Some(value) = self.state_root {
            overrides = overrides.with_state_root(value);
        }
        if let Some(value) = self.runtime_root {
            overrides = overrides.with_runtime_root(value);
        }
        if let Some(value) = self.database_path {
            overrides = overrides.with_database_path(value);
        }
        if let Some(value) = self.public_bind {
            overrides = overrides.with_public_bind(value);
        }
        if let Some(value) = self.admin_socket {
            overrides = overrides.with_admin_socket(value);
        }
        if let Some(value) = self.database_busy_timeout_ms {
            overrides = overrides.with_database_busy_timeout_ms(value);
        }
        if let Some(value) = self.database_writer_queue_capacity {
            overrides = overrides.with_database_writer_queue_capacity(value);
        }
        if let Some(value) = self.database_read_pool_size {
            overrides = overrides.with_database_read_pool_size(value);
        }
        if let Some(value) = self.content_publication_file_bytes {
            overrides = overrides.with_content_publication_file_bytes(value);
        }
        if let Some(value) = self.content_post_file_bytes {
            overrides = overrides.with_content_post_file_bytes(value);
        }
        if let Some(value) = self.content_asset_file_bytes {
            overrides = overrides.with_content_asset_file_bytes(value);
        }
        if let Some(value) = self.content_total_tree_bytes {
            overrides = overrides.with_content_total_tree_bytes(value);
        }
        if let Some(value) = self.content_entries {
            overrides = overrides.with_content_entries(value);
        }
        if let Some(value) = self.content_depth {
            overrides = overrides.with_content_depth(value);
        }
        if let Some(value) = self.content_path_bytes {
            overrides = overrides.with_content_path_bytes(value);
        }
        (self.config, overrides)
    }
}

#[derive(Debug, Args)]
pub(crate) struct AdminArguments {
    #[command(subcommand)]
    pub(crate) command: AdminCommand,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Subcommand)]
pub(crate) enum AdminCommand {
    /// Report the admin API versions supported by the running server.
    Capabilities,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_uses_the_documented_default_host_file() {
        let arguments = ProcessArguments::try_parse_from(["maincopy", "serve"]).unwrap();
        let ProcessCommand::Serve(arguments) = arguments.command else {
            panic!("serve command must parse");
        };
        let (path, _) = (*arguments).into_configuration();
        assert_eq!(path, PathBuf::from(DEFAULT_HOST_CONFIGURATION_FILE));
    }

    #[test]
    fn serve_wires_every_typed_non_secret_override() {
        let arguments = ProcessArguments::try_parse_from([
            "maincopy",
            "serve",
            "--config",
            "host.toml",
            "--content-root",
            "publication",
            "--state-root",
            "persistent",
            "--runtime-root",
            "ephemeral",
            "--database-path",
            "database.sqlite3",
            "--public-bind",
            "127.0.0.1:4000",
            "--admin-socket",
            "admin.socket",
            "--database-busy-timeout-ms",
            "7000",
            "--database-writer-queue-capacity",
            "256",
            "--database-read-pool-size",
            "8",
            "--content-publication-file-bytes",
            "131072",
            "--content-post-file-bytes",
            "2097152",
            "--content-asset-file-bytes",
            "16777216",
            "--content-total-tree-bytes",
            "134217728",
            "--content-entries",
            "5000",
            "--content-depth",
            "8",
            "--content-path-bytes",
            "512",
        ])
        .unwrap();
        let ProcessCommand::Serve(arguments) = arguments.command else {
            panic!("serve command must parse");
        };
        let (path, overrides) = (*arguments).into_configuration();
        assert_eq!(path, PathBuf::from("host.toml"));
        assert_eq!(
            overrides,
            HostConfigurationOverrides::default()
                .with_content_root(PathBuf::from("publication"))
                .with_state_root(PathBuf::from("persistent"))
                .with_runtime_root(PathBuf::from("ephemeral"))
                .with_database_path(PathBuf::from("database.sqlite3"))
                .with_public_bind("127.0.0.1:4000".parse().unwrap())
                .with_admin_socket(PathBuf::from("admin.socket"))
                .with_database_busy_timeout_ms(7_000)
                .with_database_writer_queue_capacity(256)
                .with_database_read_pool_size(8)
                .with_content_publication_file_bytes(131_072)
                .with_content_post_file_bytes(2_097_152)
                .with_content_asset_file_bytes(16_777_216)
                .with_content_total_tree_bytes(134_217_728)
                .with_content_entries(5_000)
                .with_content_depth(8)
                .with_content_path_bytes(512)
        );
    }

    #[test]
    fn serve_help_contains_no_secret_value_flags() {
        use clap::CommandFactory as _;

        let mut command = ProcessArguments::command();
        let serve = command
            .find_subcommand_mut("serve")
            .expect("serve subcommand must exist");
        let help = serve.render_long_help().to_string();
        for expected in [
            "--config <PATH>",
            "--content-publication-file-bytes <BYTES>",
            "--content-post-file-bytes <BYTES>",
            "--content-asset-file-bytes <BYTES>",
            "--content-total-tree-bytes <BYTES>",
            "--content-entries <COUNT>",
            "--content-depth <COUNT>",
            "--content-path-bytes <BYTES>",
        ] {
            assert!(help.contains(expected), "serve help omitted {expected}");
        }
        for forbidden in ["credential-value", "secret-value", "bearer-token"] {
            assert!(!help.contains(forbidden));
        }
    }
}
