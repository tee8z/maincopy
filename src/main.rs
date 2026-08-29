use maincopy::{error::ProcessExit, startup::run_until_stop};

#[tokio::main]
async fn main() -> ProcessExit {
    run_until_stop().await
}
