//! meshbot-rs — MeshCore channel rule/action bot.
//!
//! Bootstrap skeleton: connects to the radio over TCP, verifies the channel
//! map it was configured with, then streams channel messages. The rule engine,
//! `.env`/YAML config and HTTP actions are not wired up yet.

use anyhow::{Context, Result};
use futures::StreamExt;
use meshcore_rs::{EventPayload, EventType, MeshCore};

/// Physical channel index -> expected channel name.
///
/// These are the *radio's* real indices. We never send `SET_CHANNEL`, so the
/// proxy's virtualizer never maps us and the indices pass through unchanged.
const CHANNELS: &[(u8, &str)] = &[
    (3, "ruellan-family #biniou-admin"),
    (4, "ruellan-family #biniou-ha"),
];

/// How many channel slots to read back when verifying the map.
const CHANNEL_SLOTS: u8 = 8;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    dotenvy::dotenv().ok();

    let host = std::env::var("MESHCORE_HOST").unwrap_or_else(|_| "proxy".to_string());
    let port: u16 = std::env::var("MESHCORE_PORT")
        .unwrap_or_else(|_| "5000".to_string())
        .parse()
        .context("MESHCORE_PORT must be a number")?;

    tracing::info!(version = env!("CARGO_PKG_VERSION"), host = %host, port, "starting");

    let meshcore = MeshCore::tcp(&host, port)
        .await
        .with_context(|| format!("cannot connect to {host}:{port}"))?;

    let info = meshcore
        .commands()
        .lock()
        .await
        .send_appstart()
        .await
        .context("send_appstart failed")?;
    tracing::info!(name = %info.name, "app started");

    verify_channels(&meshcore).await?;

    // Returns unit, not a Result — there is nothing to await that can fail.
    meshcore.start_auto_message_fetching().await;
    tracing::info!("listening for channel messages");

    let mut stream = meshcore.event_stream_filtered(EventType::ChannelMsgRecv);
    while let Some(event) = stream.next().await {
        if let EventPayload::ChannelMessage(msg) = event.payload {
            let listening = CHANNELS.iter().any(|(idx, _)| *idx == msg.channel_idx);
            tracing::info!(
                channel_idx = msg.channel_idx,
                message_id = msg.message_id(),
                text = %msg.text,
                listening,
                "channel message"
            );
        }
    }

    Ok(())
}

/// Read the radio's channel table back and fail if it disagrees with `CHANNELS`.
///
/// Read-only: this issues `GET_CHANNEL` only. `SET_CHANNEL` would be the one
/// call that puts us in the proxy's virtualizer and remaps indices.
async fn verify_channels(meshcore: &MeshCore) -> Result<()> {
    let commands = meshcore.commands().lock().await;

    for idx in 0..CHANNEL_SLOTS {
        let info = match commands.get_channel(idx).await {
            Ok(info) => info,
            Err(err) => {
                // Empty slots are expected to fail or come back blank; only a
                // slot we actually care about is a hard failure.
                tracing::debug!(channel_idx = idx, %err, "no channel in slot");
                continue;
            }
        };

        let expected = CHANNELS.iter().find(|(i, _)| *i == idx).map(|(_, n)| *n);
        match expected {
            Some(name) if info.name == name => {
                tracing::info!(channel_idx = idx, name = %info.name, "channel verified");
            }
            Some(name) => {
                anyhow::bail!(
                    "channel {idx} is {:?} but this build expects {name:?}; \
                     the radio layout has drifted — refusing to start",
                    info.name
                );
            }
            None => tracing::info!(channel_idx = idx, name = %info.name, "unmonitored channel"),
        }
    }

    Ok(())
}
