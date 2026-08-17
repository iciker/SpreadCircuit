use crate::types::market::DexName;

#[derive(Debug, Clone)]
pub struct EvmSwapRequest {
    /// false = 卖出 token1 换 token0（buy_diff 方向）
    /// true  = 花 token0 买入 token1（sell_diff 方向）
    pub is_buy: bool,
    pub amount_in: f64,
    pub target_amount: f64,
    /// 触发套利时的最优 DEX，仅用于观测；执行前会按真实金额重新选路由。
    pub dex: DexName,
}

#[derive(Debug, Clone)]
pub enum LiquidCommand {
    Order { is_buy: bool, price: f64, size: f64 },
    Cancel { oid: u64 },
}

#[derive(Debug, Clone)]
pub enum TradeResult {
    EvmSwapSuccess {
        amount_out: f64,
        tx_hash: String,
    },
    EvmSwapFailed {
        reason: String,
    },
    /// 请求已进入执行阶段，但调用方无法确认交易是否上链，禁止自动重试。
    EvmSwapUnknown {
        reason: String,
    },
    LiquidFilled {
        oid: u64,
        avg_price: f64,
        total_size: f64,
    },
    LiquidResting {
        oid: u64,
    },
    LiquidCancelled {
        oid: u64,
    },
    LiquidFailed {
        reason: String,
    },
}
