use anyhow::{Context, Result};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::Response,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use redis::AsyncCommands;
use serde_json::Value;
use std::env;
use tokio::sync::watch;
use tracing::{error, info, warn};
use redis::streams::StreamReadReply;

struct AppState {
    subshard: String,
    redis_client: redis::Client,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let subshard = env::var("SUBSHARD").context("SUBSHARD env variable not set")?;
    let port = env::var("PORT").unwrap_or_else(|_| "4343".to_string());
    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1/".to_string());

    let redis_client =
        redis::Client::open(redis_url.as_str()).context("failed to create redis client")?;

    // Test redis connection at startup
    let mut test_conn = redis_client
        .get_multiplexed_async_connection()
        .await
        .context("failed to connect to redis")?;
    let _: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut test_conn).await;
    info!("redis connection established");

    let state = std::sync::Arc::new(AppState {
        subshard,
        redis_client,
    });

    let app = Router::new().route(
        "/",
        get({
            let state = state.clone();
            move |ws: WebSocketUpgrade| async move { ws_upgrade(ws, state) }
        }),
    );

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .context("failed to bind")?;

    info!("listening on 0.0.0.0:{}", port);
    axum::serve(listener, app).await?;

    Ok(())
}

fn ws_upgrade(ws: WebSocketUpgrade, state: std::sync::Arc<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: std::sync::Arc<AppState>) {
    if let Err(e) = handle_socket_inner(socket, state).await {
        error!("ws session ended with error: {:#}", e);
    } else {
        info!("ws session ended cleanly");
    }
}

async fn handle_socket_inner(socket: WebSocket, state: std::sync::Arc<AppState>) -> Result<()> {
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // HELLO
    let hello = serde_json::json!({
        "t": null,
        "s": null,
        "op": 10,
        "d": {
            "heartbeat_interval": 41250
        }
    });
    ws_sender
        .send(Message::Text(hello.to_string().into()))
        .await
        .context("HELLO failed")?;
    info!("HELLO sent");

    // IDENTIFY
    loop {
        match ws_receiver.next().await {
            Some(Ok(Message::Text(text))) => {
                if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                    if parsed.get("op").and_then(|v| v.as_u64()) == Some(2) {
                        info!("received IDENTIFY");
                        break;
                    }
                    // handle heartbeat received before identify
                    if parsed.get("op").and_then(|v| v.as_u64()) == Some(1) {
                        let ack = serde_json::json!({
                            "t": null,
                            "s": null,
                            "op": 11,
                            "d": null
                        });
                        ws_sender
                            .send(Message::Text(ack.to_string().into()))
                            .await
                            .context("failed to send HEARTBEAT_ACK")?;
                        continue;
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => {
                anyhow::bail!("client disconnected before IDENTIFY");
            }
            _ => continue,
        }
    }

    // READY
    let mut redis_conn = state
        .redis_client
        .get_multiplexed_async_connection()
        .await
        .context("failed to get redis connection")?;

    let ready_raw: String = redis_conn
        .get("subshard:ready")
        .await
        .context("failed to fetch subshard:ready")?;

    let mut ready_data: Value =
        serde_json::from_str(&ready_raw).context("failed to parse subshard:ready JSON")?;

    // replace resume_gateway_url
    let resume_url = env::var("RESUME_URL").unwrap_or_else(|_| "ws://localhost:4343".to_string());
    if let Some(d) = ready_data.get_mut("d") {
        if let Some(obj) = d.as_object_mut() {
            obj.insert("resume_gateway_url".to_string(), Value::String(resume_url));
        }
    }

    ws_sender
        .send(Message::Text(ready_data.to_string().into()))
        .await
        .context("failed to send READY")?;
    info!("sent READY");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let client_reader_handle = {
        let outbound_tx = outbound_tx.clone();
        let shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            while let Some(msg_result) = ws_receiver.next().await {
                match msg_result {
                    Ok(Message::Text(text)) => {
                        if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                            match parsed.get("op").and_then(|v| v.as_u64()) {
                                Some(1) => {
                                    // heartbeat ack
                                    let ack = serde_json::json!({
                                        "t": null,
                                        "s": null,
                                        "op": 11,
                                        "d": null
                                    });
                                    if outbound_tx.send(ack.to_string()).is_err() {
                                        break;
                                    }
                                }
                                _ => {
                                    // ignore
                                }
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        info!("client sent close frame");
                        break;
                    }
                    Err(e) => {
                        warn!("client read error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            let _ = shutdown_tx.send(true);
        })
    };

    let redis_reader_handle = {
        let outbound_tx = outbound_tx.clone();
        let shutdown_rx = shutdown_rx.clone();
        let redis_client = state.redis_client.clone();
        let stream_key = format!("subshard:{}", state.subshard);
        tokio::spawn(async move {
            let mut redis_conn = match redis_client.get_multiplexed_async_connection().await {
                Ok(c) => c,
                Err(e) => {
                    error!("failed to get redis connection for stream: {}", e);
                    return;
                }
            };

            let mut last_id = "$".to_string();

            loop {
                let result: redis::RedisResult<StreamReadReply> = redis::cmd("XREAD")
                    .arg("BLOCK")
                    .arg(10_u64)
                    .arg("STREAMS")
                    .arg(&stream_key)
                    .arg(&last_id)
                    .query_async(&mut redis_conn)
                    .await;

                if *shutdown_rx.borrow() {
                    break;
                }

                match result {
                    Ok(reply) => {
                        for entry in reply.keys.into_iter().flat_map(|k| k.ids) {
                            last_id = entry.id.clone();

                            if let Some(Ok(event_str)) = entry.map.get("event").map(|v| redis::from_redis_value::<String>(v.clone())) {
                                if outbound_tx.send(event_str).is_err() { return; }
                            }
                        }
                    }
                    Err(e) => {
                        error!("redis XREAD error: {}", e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    }
                }
            }
        })
    };


    drop(outbound_tx); 

    let sender_handle = {
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg = outbound_rx.recv() => {
                        match msg {
                            Some(text) => {
                                if let Err(e) = ws_sender.send(Message::Text(text.into())).await {
                                    error!("failed to send ws message: {}", e);
                                    break;
                                }
                            }
                            None => {
                                // channel closed
                                break;
                            }
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
            // send close frame
            let _ = ws_sender.close().await;
        })
    };

    let _ = tokio::join!(client_reader_handle, redis_reader_handle, sender_handle);

    Ok(())
}