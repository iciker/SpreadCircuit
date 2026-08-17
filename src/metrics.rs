use std::sync::LazyLock;

use prometheus::{
    register_counter_vec, register_gauge, register_gauge_vec, register_histogram_vec, CounterVec,
    Encoder, Gauge, GaugeVec, HistogramVec, TextEncoder,
};

// ── 价差 ──────────────────────────────────────────────────────────────────
static PRICE_DIFF_BPS: LazyLock<GaugeVec> = LazyLock::new(|| {
    register_gauge_vec!(
        "arb_price_diff_bps",
        "Current arbitrage price diff in bps",
        &["direction", "pair"]
    )
    .unwrap()
});

static MIN_PROFIT_BPS: LazyLock<GaugeVec> = LazyLock::new(|| {
    register_gauge_vec!(
        "arb_min_profit_bps",
        "Current minimum profit threshold in bps",
        &["pair"]
    )
    .unwrap()
});

// ── 套利触发 ──────────────────────────────────────────────────────────────
static ARB_TRIGGER_TOTAL: LazyLock<CounterVec> = LazyLock::new(|| {
    register_counter_vec!(
        "arb_trigger_total",
        "Total arbitrage triggers that sent a real trade",
        &["direction", "pair"]
    )
    .unwrap()
});

static ARB_DRY_RUN_TRIGGER_TOTAL: LazyLock<CounterVec> = LazyLock::new(|| {
    register_counter_vec!(
        "arb_dry_run_trigger_total",
        "Arbitrage condition met count in dry-run mode",
        &["direction", "pair"]
    )
    .unwrap()
});

// ── EVM Swap ──────────────────────────────────────────────────────────────
static EVM_SWAP_TOTAL: LazyLock<CounterVec> = LazyLock::new(|| {
    register_counter_vec!(
        "evm_swap_total",
        "Total EVM swap attempts by result",
        &["result", "pair"]
    )
    .unwrap()
});

/// 执行前实时报价后因价格恶化而放弃的次数（未发送链上 tx）
static REQUOTE_ABORTED_TOTAL: LazyLock<CounterVec> = LazyLock::new(|| {
    register_counter_vec!(
        "evm_requote_aborted_total",
        "Swaps aborted after re-quote because price deteriorated below target",
        &["pair"]
    )
    .unwrap()
});

static EVM_SWAP_AMOUNT_OUT_TOTAL: LazyLock<CounterVec> = LazyLock::new(|| {
    register_counter_vec!(
        "evm_swap_amount_out_total",
        "Total token amount received from all EVM swaps (buy_diff: USDC, sell_diff: token1; 单位不同，勿跨 direction 聚合)",
        &["direction", "pair"]
    )
    .unwrap()
});

/// 进入 RecoveryRequired 的次数——最关键的人工介入告警信号
static ARB_RECOVERY_REQUIRED_TOTAL: LazyLock<CounterVec> = LazyLock::new(|| {
    register_counter_vec!(
        "arb_recovery_required_total",
        "Times a pair entered RecoveryRequired and stopped trading (requires manual intervention)",
        &["pair"]
    )
    .unwrap()
});

// ── Liquid 订单 ───────────────────────────────────────────────────────────
static LIQUID_ORDER_TOTAL: LazyLock<CounterVec> = LazyLock::new(|| {
    register_counter_vec!(
        "liquid_order_total",
        "Total Liquid order attempts by result",
        &["result", "pair"]
    )
    .unwrap()
});

static LIQUID_FILLED_SIZE_TOTAL: LazyLock<CounterVec> = LazyLock::new(|| {
    register_counter_vec!(
        "liquid_filled_size_total",
        "Total filled size across all Liquid orders",
        &["pair"]
    )
    .unwrap()
});

// ── 延迟 ──────────────────────────────────────────────────────────────────
/// 套利全链路耗时（触发 EvmSwap → 收到 Liquid 结果），秒，按 pair 分维度
static ARB_EXECUTION_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "arb_execution_duration_seconds",
        "End-to-end arb cycle time: from EVM swap trigger to liquid order result",
        &["pair"],
        vec![0.1, 0.5, 1.0, 2.0, 3.0, 5.0, 10.0, 20.0, 30.0]
    )
    .unwrap()
});

/// execution_gate 抢锁等待时间，按 pair 分维度
static EXECUTION_GATE_WAIT: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "evm_execution_gate_wait_seconds",
        "Time spent waiting to acquire the shared EVM execution gate before a swap",
        &["pair"],
        vec![0.001, 0.01, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0]
    )
    .unwrap()
});

/// HyperLiquid l2Book 推送延迟（服务端时间戳 → 本地收到，ms），按 pair 分维度
static HL_ORDERBOOK_LATENCY_MS: LazyLock<GaugeVec> = LazyLock::new(|| {
    register_gauge_vec!(
        "hl_orderbook_latency_ms",
        "HyperLiquid l2Book push latency in milliseconds (received_at - server created_at)",
        &["pair"]
    )
    .unwrap()
});

// ── 原始价格 ──────────────────────────────────────────────────────────────
/// EVM DEX 聚合最优价（USDC per token1），side=buy|sell，按 pair 分维度
static EVM_PRICE_USDC: LazyLock<GaugeVec> = LazyLock::new(|| {
    register_gauge_vec!(
        "evm_price_usdc",
        "Best EVM DEX price in USDC per token1 (buy: best sell quote / token1, sell: best buy quote / token1)",
        &["side", "pair"]
    )
    .unwrap()
});

/// HyperLiquid 订单簿价格（USDC per token1），side=ask|bid，按 pair 分维度
static HL_PRICE_USDC: LazyLock<GaugeVec> = LazyLock::new(|| {
    register_gauge_vec!(
        "hl_price_usdc",
        "HyperLiquid orderbook price in USDC per token1",
        &["side", "pair"]
    )
    .unwrap()
});

/// 最新处理的 EVM 区块号（用于判断 EVM 数据是否滞后）
static EVM_LATEST_BLOCK: LazyLock<Gauge> = LazyLock::new(|| {
    register_gauge!(
        "evm_latest_block_number",
        "Latest EVM block number processed by EvmWatcher"
    )
    .unwrap()
});

/// 启动时注册无标签指标；含 pair 标签的指标在首次埋点时自动注册。
pub fn init() {
    let _ = &*EVM_LATEST_BLOCK;
}

/// 返回 Prometheus text format 文本
pub fn gather() -> String {
    let encoder = TextEncoder::new();
    let mut buffer = Vec::with_capacity(4096);
    if let Err(e) = encoder.encode(&prometheus::gather(), &mut buffer) {
        tracing::warn!(error = %e, "[Metrics] 编码失败");
        return String::new();
    }
    match String::from_utf8(buffer) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "[Metrics] UTF-8 转换失败");
            String::new()
        }
    }
}

// ── 语义化埋点函数 ────────────────────────────────────────────────────────
// Actor 通过这些函数汇报事件，不直接操作全局指标对象

pub fn record_price_diff(pair: &str, buy_diff: f64, sell_diff: f64, min_bps: f64) {
    PRICE_DIFF_BPS
        .with_label_values(&["buy", pair])
        .set(buy_diff);
    PRICE_DIFF_BPS
        .with_label_values(&["sell", pair])
        .set(sell_diff);
    MIN_PROFIT_BPS.with_label_values(&[pair]).set(min_bps);
}

pub fn record_arb_triggered(pair: &str, direction: &str) {
    ARB_TRIGGER_TOTAL
        .with_label_values(&[direction, pair])
        .inc();
}

pub fn record_arb_dry_run_triggered(pair: &str, direction: &str) {
    ARB_DRY_RUN_TRIGGER_TOTAL
        .with_label_values(&[direction, pair])
        .inc();
}

pub fn record_evm_swap_success(pair: &str, direction: &str, amount_out: f64) {
    EVM_SWAP_TOTAL.with_label_values(&["success", pair]).inc();
    EVM_SWAP_AMOUNT_OUT_TOTAL
        .with_label_values(&[direction, pair])
        .inc_by(amount_out);
}

pub fn record_evm_swap_failed(pair: &str) {
    EVM_SWAP_TOTAL.with_label_values(&["failed", pair]).inc();
}

/// 发送 tx 之前安全放弃（链上零资金变动）
pub fn record_evm_swap_aborted(pair: &str) {
    EVM_SWAP_TOTAL.with_label_values(&["aborted", pair]).inc();
}

/// 执行结果不明（超时/链上成功但金额未知），需人工核对
pub fn record_evm_swap_unknown(pair: &str) {
    EVM_SWAP_TOTAL.with_label_values(&["unknown", pair]).inc();
}

/// 进入 RecoveryRequired，pair 停止交易——应配置最高优先级告警
pub fn record_arb_recovery_required(pair: &str) {
    ARB_RECOVERY_REQUIRED_TOTAL.with_label_values(&[pair]).inc();
}

/// 执行前 re-quote 发现价格恶化、放弃 swap（未发链上 tx）
pub fn record_requote_aborted(pair: &str) {
    REQUOTE_ABORTED_TOTAL.with_label_values(&[pair]).inc();
}

pub fn record_liquid_order_filled(pair: &str, total_size: f64) {
    LIQUID_ORDER_TOTAL
        .with_label_values(&["filled", pair])
        .inc();
    LIQUID_FILLED_SIZE_TOTAL
        .with_label_values(&[pair])
        .inc_by(total_size);
}

pub fn record_liquid_order_resting(pair: &str) {
    LIQUID_ORDER_TOTAL
        .with_label_values(&["resting", pair])
        .inc();
}

pub fn record_liquid_order_failed(pair: &str) {
    LIQUID_ORDER_TOTAL
        .with_label_values(&["failed", pair])
        .inc();
}

/// 记录 execution_gate 抢锁等待时间
pub fn record_execution_gate_wait(pair: &str, secs: f64) {
    EXECUTION_GATE_WAIT.with_label_values(&[pair]).observe(secs);
}

/// 记录套利全链路耗时（从发出 EvmSwap 到收到 Liquid 结果）
pub fn record_arb_execution_duration(pair: &str, secs: f64) {
    ARB_EXECUTION_DURATION
        .with_label_values(&[pair])
        .observe(secs);
}

/// 记录 HyperLiquid l2Book 推送延迟（按 pair 分维度，避免多 pair 数据相互覆盖）
pub fn record_hl_orderbook_latency(pair: &str, ms: f64) {
    HL_ORDERBOOK_LATENCY_MS.with_label_values(&[pair]).set(ms);
}

/// 记录 EVM DEX 聚合最优原始价格（每 token1 对应 USDC，以 QUOTE_AMOUNT=100 反算）
pub fn record_evm_price(pair: &str, buy_price: f64, sell_price: f64) {
    EVM_PRICE_USDC
        .with_label_values(&["buy", pair])
        .set(buy_price);
    EVM_PRICE_USDC
        .with_label_values(&["sell", pair])
        .set(sell_price);
}

/// 记录 HyperLiquid 订单簿 ask/bid 原始价格
pub fn record_hl_price(pair: &str, ask: f64, bid: f64) {
    HL_PRICE_USDC.with_label_values(&["ask", pair]).set(ask);
    HL_PRICE_USDC.with_label_values(&["bid", pair]).set(bid);
}

/// 记录最新处理的 EVM 区块号
pub fn record_evm_block(block_number: u64) {
    EVM_LATEST_BLOCK.set(block_number as f64);
}
