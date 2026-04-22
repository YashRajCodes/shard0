use futures_util::StreamExt;
use serde_json::Value;
use std::env;
use twilight_gateway::{Config, Intents, Message, Shard, ShardId};

const TOTAL_SUBSHARDS: u64 = 4;
const TOTAL_SHARDS: u32 = 2;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    rustls::crypto::ring::default_provider().install_default().unwrap();

    let token = env::var("DISCORD_TOKEN")?;
    let config = Config::new(token, Intents::all());
    
    let mut shard = Shard::with_config(ShardId::new(0, TOTAL_SHARDS), config);

    tracing::info!("shard 0/{} -> {} subshards", TOTAL_SHARDS, TOTAL_SUBSHARDS);

    while let Some(item) = shard.next().await {
        match item {
            Ok(Message::Text(text)) => {
                let json: Value = serde_json::from_str(&text)?;

                if json["op"] == 0 {
                    let event_type = json["t"].as_str().unwrap_or("UNKNOWN");
                    let data = &json["d"];

                    let id_str = data["channel_id"]
                        .as_str()
                        .or_else(|| data["id"].as_str());

                    let target_subshard = id_str
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(|id_u64| id_u64 % TOTAL_SUBSHARDS)
                        .unwrap_or(0);

                    tracing::debug!(
                        "{}:{} -> {}",
                        event_type,
                        id_str.unwrap_or("NONE"),
                        target_subshard
                    );
                }
            }
            Ok(_) => {}
            Err(source) => {
                tracing::warn!(?source, "gateway connection issue");
            }
        }
    }

    Ok(())
}