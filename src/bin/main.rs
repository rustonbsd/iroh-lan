use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {

    if !self_runas::is_elevated() {
        self_runas::admin()?;
        return Ok(());
    }

    return iroh_lan::cli::run_cli().await
}
