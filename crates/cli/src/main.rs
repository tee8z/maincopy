#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    maincopy_cli::run().await
}
