use gateway_plugin_sdk::client::{PluginSession, SessionConfig};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session = PluginSession::accept(
        tokio::io::stdin(),
        tokio::io::stdout(),
        SessionConfig::default(),
    )
    .await?;
    session.run(cpr_plugin_cx_panel::plugin()?).await?;
    Ok(())
}
