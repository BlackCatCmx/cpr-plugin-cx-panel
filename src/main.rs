use gateway_plugin_sdk::client::{PluginSession, SessionConfig};
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session = PluginSession::accept(
        tokio::io::stdin(),
        tokio::io::stdout(),
        SessionConfig::default(),
    )
    .await?;
    let config = serde_json::from_value(session.handshake().configuration.clone())?;
    let admin = Arc::new(cpr_plugin_cx_panel::AdminClient::new(config)?);
    session.run(cpr_plugin_cx_panel::plugin(admin)?).await?;
    Ok(())
}
