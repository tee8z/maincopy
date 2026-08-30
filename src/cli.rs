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
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
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
        let overrides = HostConfigurationOverrides {
            content_root: self.content_root,
            state_root: self.state_root,
            runtime_root: self.runtime_root,
            database_path: self.database_path,
            public_bind: self.public_bind,
            admin_socket: self.admin_socket,
            database_busy_timeout_ms: self.database_busy_timeout_ms,
            database_writer_queue_capacity: self.database_writer_queue_capacity,
            database_read_pool_size: self.database_read_pool_size,
            content_publication_file_bytes: self.content_publication_file_bytes,
            content_post_file_bytes: self.content_post_file_bytes,
            content_asset_file_bytes: self.content_asset_file_bytes,
            content_total_tree_bytes: self.content_total_tree_bytes,
            content_entries: self.content_entries,
            content_depth: self.content_depth,
            content_path_bytes: self.content_path_bytes,
        };
        (self.config, overrides)
    }
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
            HostConfigurationOverrides {
                content_root: Some(PathBuf::from("publication")),
                state_root: Some(PathBuf::from("persistent")),
                runtime_root: Some(PathBuf::from("ephemeral")),
                database_path: Some(PathBuf::from("database.sqlite3")),
                public_bind: Some("127.0.0.1:4000".parse().unwrap()),
                admin_socket: Some(PathBuf::from("admin.socket")),
                database_busy_timeout_ms: Some(7_000),
                database_writer_queue_capacity: Some(256),
                database_read_pool_size: Some(8),
                content_publication_file_bytes: Some(131_072),
                content_post_file_bytes: Some(2_097_152),
                content_asset_file_bytes: Some(16_777_216),
                content_total_tree_bytes: Some(134_217_728),
                content_entries: Some(5_000),
                content_depth: Some(8),
                content_path_bytes: Some(512),
            }
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
