use std::fmt;

/// EVM 上执行报价和 swap 的 DEX 标识
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DexName {
    Prjx,
    HyperSwap,
    Kitten,
}

impl DexName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prjx => "prjx",
            Self::HyperSwap => "hyperswap",
            Self::Kitten => "kitten",
        }
    }
}

impl fmt::Display for DexName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct EvmPrice {
    pub buy_price: f64,
    pub sell_price: f64,
    pub block_number: u64,
    pub received_at: u64,
    /// 给出最优 sell 报价（token1→token0）的 DEX，执行 buy_diff swap 时路由到这里
    pub sell_dex: DexName,
    /// 给出最优 buy 报价（token0→token1）的 DEX，执行 sell_diff swap 时路由到这里
    pub buy_dex: DexName,
    pub pair_id: String, // pair 标识符，与 PairConfig::symbol 一致
}

pub fn unix_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone)]
pub struct OrderBook {
    pub bid: f64,
    pub ask: f64,
    pub created_at: u64,
    pub received_at: u64,
    pub symbol: String, // pair 标识符，与 PairConfig::symbol 一致
}

#[derive(Debug, Clone)]
pub struct PriceRecord {
    pub pair: String,
    pub evm_buy_price: f64,
    pub evm_sell_price: f64,
    pub liquid_ask: f64,
    pub liquid_bid: f64,
    pub sell_diff: f64,
    pub buy_diff: f64,
}
