use tokio::sync::mpsc;
use tracing::{error, info};

use crate::{
    liquid::client::{LiquidClient, OrderStatus},
    types::commands::{LiquidCommand, TradeResult},
};

pub async fn run(
    client: LiquidClient,
    symbol: String,
    mut rx: mpsc::Receiver<LiquidCommand>,
    tx: mpsc::Sender<TradeResult>,
) {
    while let Some(cmd) = rx.recv().await {
        let result = match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            handle_command(&client, &symbol, cmd),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => TradeResult::LiquidFailed {
                reason: "HyperLiquid 请求超时".into(),
            },
        };
        if tx.send(result).await.is_err() {
            error!("[LiquidTrader] result channel closed");
            break;
        }
    }
    info!(symbol, "[LiquidTrader] command channel 已关闭，退出");
}

async fn handle_command(client: &LiquidClient, symbol: &str, cmd: LiquidCommand) -> TradeResult {
    match cmd {
        LiquidCommand::Order {
            is_buy,
            price,
            size,
        } => match client.market_order(is_buy, price, size).await {
            Ok(r) => match r.status {
                OrderStatus::Filled => {
                    let (Some(oid), Some(avg_price), Some(total_size)) =
                        (r.oid, r.avg_price, r.total_size)
                    else {
                        error!(
                            symbol,
                            "[LiquidTrader] Filled 响应缺少 oid/avg_price/total_size"
                        );
                        crate::metrics::record_liquid_order_failed(symbol);
                        return TradeResult::LiquidFailed {
                            reason: "filled response missing execution fields".into(),
                        };
                    };
                    crate::metrics::record_liquid_order_filled(symbol, total_size);
                    TradeResult::LiquidFilled {
                        oid,
                        avg_price,
                        total_size,
                    }
                }
                OrderStatus::Resting => match r.oid {
                    Some(oid) => {
                        crate::metrics::record_liquid_order_resting(symbol);
                        TradeResult::LiquidResting { oid }
                    }
                    None => {
                        error!(symbol, "[LiquidTrader] Resting 但 oid 缺失，无法发起撤单");
                        crate::metrics::record_liquid_order_failed(symbol);
                        TradeResult::LiquidFailed {
                            reason: "resting order missing oid".into(),
                        }
                    }
                },
                _ => {
                    crate::metrics::record_liquid_order_failed(symbol);
                    TradeResult::LiquidFailed {
                        reason: "unknown status".into(),
                    }
                }
            },
            Err(e) => {
                crate::metrics::record_liquid_order_failed(symbol);
                TradeResult::LiquidFailed {
                    reason: e.to_string(),
                }
            }
        },
        LiquidCommand::Cancel { oid } => match client.cancel_order(oid).await {
            Ok(_) => TradeResult::LiquidCancelled { oid },
            Err(e) => TradeResult::LiquidFailed {
                reason: e.to_string(),
            },
        },
    }
}
