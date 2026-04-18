mod server;

use dap_core::transport::{DapReader, DapWriter};
use serde_json::Value;
use server::Backend;
use tokio::sync::mpsc::unbounded_channel;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_env_filter("debug")
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .init();

    let (tx, mut rx) = unbounded_channel::<Value>();

    tokio::spawn(async move {
        let mut writer = DapWriter::new(tokio::io::stdout());
        while let Some(msg) = rx.recv().await {
            if let Err(e) = writer.send(&msg).await {
                tracing::error!("write error: {e}");
                break;
            }
        }
    });

    let backend = Backend::new(tx);
    let mut reader = DapReader::new(tokio::io::stdin());

    loop {
        match reader.read_message().await {
            Ok(msg) => backend.handle_message(msg).await,
            Err(e) => {
                tracing::error!("read error: {e}");
                break;
            }
        }
    }
}
