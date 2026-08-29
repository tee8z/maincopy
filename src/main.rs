use maincopy::startup::run_until_stop;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_until_stop().await
}
