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
/// 无任何消息超过此时长视为半开连接，断开重连（l2Book 每个块都有推送）
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// 关闭期间轮询订阅者数量的间隔
const SHUTDOWN_POLL: std::time::Duration = std::time::Duration::from_millis(200);

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
        &config,
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
        // 收到关闭信号后不立即退出：在途套利的第二腿仍需要新鲜订单簿完成对冲，
        // 等所有 ArbEngine 退出（订阅者归零）后再停止推送
        if shutdown.is_cancelled() && tx.receiver_count() == 0 {
            return Ok(());
        }
        tokio::select! {
            msg = tokio::time::timeout(IDLE_TIMEOUT, ws.next()) => {
                match msg {
                    Ok(Some(Ok(Message::Text(text)))) => {
                        if let Some(ob) = parse_orderbook(&text) {
                            if tx.send(ob).is_err() && !shutdown.is_cancelled() {
                                warn!("[OrderBookWatcher] 无订阅者，orderbook 已丢弃");
                            }
                        } else if text.contains("\"channel\":\"error\"") {
                            // 订阅被拒绝等服务端错误必须可见，否则 pair 永久无数据且无告警
                            warn!(message = %text, "[OrderBookWatcher] 服务端错误消息");
                        }
                    }
                    Ok(Some(Err(e))) => return Err(e.into()),
                    Ok(None) => return Err(anyhow::anyhow!("WS stream ended")),
                    Ok(_) => {}
                    Err(_) => {
                        return Err(anyhow::anyhow!(
                            "WS 超过 {IDLE_TIMEOUT:?} 无消息，判定半开连接，重连"
                        ))
                    }
                }
            }
            _ = shutdown.cancelled(), if !shutdown.is_cancelled() => {}
            _ = tokio::time::sleep(SHUTDOWN_POLL), if shutdown.is_cancelled() => {}
        }
    }
}

pub fn parse_orderbook(text: &str) -> Option<OrderBook> {
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
