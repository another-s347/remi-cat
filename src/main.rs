#[tokio::main]
async fn main() -> anyhow::Result<()> {
    remi_cat::run_cli().await
}
