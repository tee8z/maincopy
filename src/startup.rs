use clap::{Parser, Subcommand};

/// Owns Maincopy's process-level resources and lifecycle.
///
/// Configuration validation, dependency construction, task supervision, and
/// graceful shutdown will live here as the service is implemented.
pub struct Application;

#[derive(Debug, Parser)]
#[command(
    name = "maincopy",
    version,
    about = "One canonical copy. Every channel."
)]
struct ProcessArguments {
    #[command(subcommand)]
    command: ProcessCommand,
}

#[derive(Debug, Subcommand)]
enum ProcessCommand {
    /// Run the public site, admin API, scheduler, and workers.
    Serve,
}

pub async fn run_until_stop() -> anyhow::Result<()> {
    dispatch(ProcessArguments::parse()).await
}

async fn dispatch(arguments: ProcessArguments) -> anyhow::Result<()> {
    match arguments.command {
        ProcessCommand::Serve => {
            let application = Application::build().await?;
            application.run_until_stop().await
        }
    }
}

impl Application {
    async fn build() -> anyhow::Result<Self> {
        Ok(Self)
    }

    async fn run_until_stop(self) -> anyhow::Result<()> {
        println!("Maincopy is being built. See IMPLEMENTATION.md for the v1 plan.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, error::ErrorKind};

    use super::ProcessArguments;

    #[test]
    fn help_is_available_without_building_the_application() {
        let error = ProcessArguments::command()
            .try_get_matches_from(["maincopy", "--help"])
            .expect_err("help stops command dispatch");

        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
    }
}
