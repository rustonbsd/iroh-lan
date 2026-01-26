use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    iroh_lan::cli::run_cli().await
}