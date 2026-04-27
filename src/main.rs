use futures_util::StreamExt;
use redis::AsyncCommands;
use serde::Deserialize;
use std::env;
use twilight_gateway::{Config, Intents, Message, Shard, ShardId};

#[derive(Deserialize)]
struct GatewayPayload {
    op: u8,
    #[serde(default)]
    t: Option<String>,
    #[serde(default)]
    d: Option<EventData>,
}

#[derive(Deserialize)]
struct EventData {
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    rustls::crypto::ring::default_provider().install_default().unwrap();

    let total_subshards: u64 = env::var("TOTAL_SUBSHARDS").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
    let total_shards: u32 = env::var("TOTAL_SHARDS").ok().and_then(|v| v.parse().ok()).unwrap_or(2);

    let token = env::var("DISCORD_TOKEN")?;
    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1/".to_string());

    let redis_client = redis::Client::open(redis_url)?;
    let redis_conn = redis_client.get_multiplexed_async_connection().await?;

    let config = Config::new(token, Intents::all());
    let mut shard = Shard::with_config(ShardId::new(0, total_shards), config);

    tracing::info!("shard 0/{} -> {} subshards", total_shards, total_subshards);

    while let Some(item) = shard.next().await {
        match item {
            Ok(Message::Text(text)) => {
                if let Err(e) = process_message(text, redis_conn.clone(), total_subshards).await {
                    tracing::warn!(?e, "failed to process message");
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

async fn process_message(
    text: String,
    mut redis_conn: redis::aio::MultiplexedConnection,
    total_subshards: u64,
) -> anyhow::Result<()> {
    let payload: GatewayPayload = serde_json::from_slice(text.as_bytes())?;

    if payload.op == 0 {
        if let (Some(event_type), Some(data)) = (payload.t, payload.d) {
            if event_type == "READY" {
                redis_conn.set::<_, _, ()>("subshard:ready", &text).await?;
                tracing::info!("subshard:ready updated");
                return Ok(());
            }

            let id_str = data.channel_id.as_deref().or(data.id.as_deref());

            let target_subshard = id_str
                .and_then(|s| s.parse::<u64>().ok())
                .map(|id_u64| id_u64 % total_subshards)
                .unwrap_or(0);

            tracing::debug!(
                "{}:{} -> {}",
                event_type,
                id_str.unwrap_or("NONE"),
                target_subshard
            );

            let stream_key = format!("subshard:{}", target_subshard);

            redis_conn
                .xadd_maxlen::<_, _, _, _, ()>(
                    &stream_key,
                    redis::streams::StreamMaxlen::Approx(1000),
                    "*",
                    &[("event", text.as_str())],
                )
                .await?;
        }
    }

    Ok(())
}