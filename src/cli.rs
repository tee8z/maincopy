use clap::{Args, Parser, Subcommand};

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
    Serve,

    /// Send an operation to the running server's private admin API.
    Admin(AdminArguments),
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
