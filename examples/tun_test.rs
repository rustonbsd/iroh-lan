use tokio::signal::ctrl_c;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if !self_runas::is_elevated() {
        self_runas::admin()?;
        return Ok(());
    }

    let (remote_writer,_remote_reader) = tokio::sync::mpsc::channel(1024);
    let _t = iroh_lan::Tun::new((0,5),remote_writer)?;


    ctrl_c().await.unwrap();
    Ok(())
}
