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
    /// 发送 tx 之前的安全失败（re-quote 失败、盈利消失、余额不足、配置错误）：
    /// 链上零资金变动，可直接放弃本轮，无需人工恢复。
    EvmSwapAborted {
        reason: String,
    },
    /// tx 已（或可能已）发送后的失败：状态不明或已回滚，必须 fail-closed。
    EvmSwapFailed {
        reason: String,
    },
    /// 链上已确认但实际成交金额未知（Transfer 事件缺失）：资金已划转，
    /// 必须继续对冲，禁止重试第一腿。
    EvmSwapConfirmedAmountUnknown {
        tx_hash: String,
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
        /// true = 交易所明确拒单（订单从未上簿），可用最新订单簿安全重试一次；
        /// false = 传输层错误/超时，订单状态不明，禁止重试。
        retriable: bool,
    },
}
