use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    config::Config,
    types::market::{unix_timestamp_ms, OrderBook},
};

const CHANNEL_L2BOOK: &str = "l2Book";
/// 用于 JSON 文本的廉价预筛选，包含引号以避免匹配非 channel 字段
const CHANNEL_L2BOOK_QUOTED: &str = "\"l2Book\"";

/// 订阅多个 symbol 的订单簿，共享单一 WebSocket 连接
pub async fn run(
    config: Arc<Config>,
    symbols: Vec<String>,
    tx: broadcast::Sender<OrderBook>,
    shutdown: CancellationToken,
) {
    super::run_with_backoff(
        || connect_and_listen(&config, &symbols, &tx, &shutdown),
        "OrderBookWatcher",
        &shutdown,
    )
    .await;
    info!("[OrderBookWatcher] 已退出");
}

async fn connect_and_listen(
    config: &Config,
    symbols: &[String],
    tx: &broadcast::Sender<OrderBook>,
    shutdown: &CancellationToken,
) -> Result<()> {
    let (mut ws, _) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        connect_async(&config.wss_api),
    )
    .await
    .map_err(|_| anyhow::anyhow!("WebSocket 连接超时"))??;
    info!(symbols = ?symbols, "[OrderBookWatcher] WebSocket 已连接");

    for symbol in symbols {
        ws.send(Message::Text(
            json!({
                "method": "subscribe",
                "subscription": { "type": CHANNEL_L2BOOK, "coin": symbol }
            })
            .to_string(),
        ))
        .await?;
    }

    loop {
        tokio::select! {
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(ob) = parse_orderbook(&text) {
                            if tx.send(ob).is_err() {
                                warn!("[OrderBookWatcher] 无订阅者，orderbook 已丢弃");
                            }
                        }
                    }
                    Some(Err(e)) => return Err(e.into()),
                    None => return Err(anyhow::anyhow!("WS stream ended")),
                    _ => {}
                }
            }
            _ = shutdown.cancelled() => return Ok(()),
        }
    }
}

pub fn parse_orderbook(text: &str) -> Option<OrderBook> {
    // 廉价字符串预筛选，避免对非 l2Book 消息做完整 JSON 解析
    if !text.contains(CHANNEL_L2BOOK_QUOTED) {
        return None;
    }
    let v: Value = serde_json::from_str(text).ok()?;
    if v["channel"] != CHANNEL_L2BOOK {
        return None;
    }

    let data = &v["data"];
    let symbol = data["coin"].as_str()?.to_string();
    let bid = data["levels"][0][0]["px"].as_str()?.parse::<f64>().ok()?;
    let ask = data["levels"][1][0]["px"].as_str()?.parse::<f64>().ok()?;
    if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask <= 0.0 || bid > ask {
        return None;
    }
    let created_at = data["time"].as_u64().unwrap_or(0);

    Some(OrderBook {
        bid,
        ask,
        created_at,
        received_at: unix_timestamp_ms(),
        symbol,
    })
}
