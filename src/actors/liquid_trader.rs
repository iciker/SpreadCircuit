use tokio::sync::mpsc;
use tracing::{error, info};

use crate::{
    liquid::client::{LiquidClient, OrderStatus},
    types::commands::{LiquidCommand, TradeResult},
};

/// 失败结果统一出口：埋点与结果构造成对出现，新增分支不会漏埋点
fn fail(symbol: &str, reason: impl Into<String>, retriable: bool) -> TradeResult {
    crate::metrics::record_liquid_order_failed(symbol);
    TradeResult::LiquidFailed {
        reason: reason.into(),
        retriable,
    }
}

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
            // 超时后订单状态不明，禁止重试
            Err(_) => fail(&symbol, "HyperLiquid 请求超时", false),
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
                        // 订单已成交只是字段缺失，重试会双重对冲
                        return fail(symbol, "filled response missing execution fields", false);
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
                        // 订单已上簿，重试会双重对冲
                        fail(symbol, "resting order missing oid", false)
                    }
                },
                // 交易所明确拒单：订单从未上簿，允许调用方重试一次
                OrderStatus::Rejected => fail(
                    symbol,
                    "exchange rejected order（详见 LiquidClient 日志）",
                    true,
                ),
                // 状态不明：禁止重试
                OrderStatus::Failed | OrderStatus::Unknown => fail(symbol, "unknown status", false),
            },
            // 传输层错误，订单可能已被接收
            Err(e) => fail(symbol, e.to_string(), false),
        },
        LiquidCommand::Cancel { oid } => match client.cancel_order(oid).await {
            Ok(_) => TradeResult::LiquidCancelled { oid },
            // 撤单失败，resting 单状态不明
            Err(e) => fail(symbol, e.to_string(), false),
        },
    }
}
