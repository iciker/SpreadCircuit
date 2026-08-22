use anyhow::Context;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{
    config::{Config, PairConfig},
    db::{self, RecoveryRecord},
    liquid::client::{normalize_spot_order, LiquidMarketRules},
    types::{
        commands::{EvmSwapRequest, LiquidCommand, TradeResult},
        market::{unix_timestamp_ms, DexName, EvmPrice, OrderBook, PriceRecord},
    },
};

use super::BPS;

const MAX_MARKET_AGE_MS: u64 = 5_000;
const MAX_MARKET_SKEW_MS: u64 = 3_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ArbDirection {
    /// EVM 卖出 token1 → HL 买入 token1（buy_diff 方向）
    BuyDiff,
    /// EVM 买入 token1 ← HL 卖出 token1（sell_diff 方向）
    SellDiff,
}

impl ArbDirection {
    pub fn label(self) -> &'static str {
        match self {
            Self::BuyDiff => "buy_diff",
            Self::SellDiff => "sell_diff",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArbState {
    Idle,
    EvmSwapping,
    LiquidOrdering,
    RecoveryRequired,
}

impl ArbState {
    pub fn can_start_quote(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// 第二腿的最坏可成交价格，已包含配置的 IOC 限价偏移。
pub fn worst_case_liquid_price(
    direction: ArbDirection,
    bid: f64,
    ask: f64,
    limit_slippage_bps: f64,
) -> anyhow::Result<f64> {
    anyhow::ensure!(
        bid.is_finite() && ask.is_finite() && bid > 0.0 && ask > 0.0 && bid <= ask,
        "订单簿必须是有限、正数且 bid <= ask"
    );
    anyhow::ensure!(
        limit_slippage_bps.is_finite() && (0.0..BPS).contains(&limit_slippage_bps),
        "LIMIT_SLIPPAGE 必须位于 [0, 10000)"
    );
    Ok(match direction {
        ArbDirection::BuyDiff => ask * (1.0 + limit_slippage_bps / BPS),
        ArbDirection::SellDiff => bid * (1.0 - limit_slippage_bps / BPS),
    })
}

fn validate_economic_terms(
    amount: f64,
    amount_name: &str,
    worst_liquid_price: f64,
    liquid_fee_bps: f64,
    profit_buffer_bps: f64,
    gas_cost_usdc: f64,
) -> anyhow::Result<()> {
    anyhow::ensure!(amount.is_finite() && amount > 0.0, "{amount_name} 数值无效");
    anyhow::ensure!(
        worst_liquid_price.is_finite() && worst_liquid_price > 0.0,
        "worst_liquid_price 数值无效"
    );
    anyhow::ensure!(
        liquid_fee_bps.is_finite() && (0.0..BPS).contains(&liquid_fee_bps),
        "liquid_fee_bps 必须位于 [0, 10000)"
    );
    anyhow::ensure!(
        profit_buffer_bps.is_finite() && (0.0..BPS).contains(&profit_buffer_bps),
        "profit_buffer_bps 必须位于 [0, 10000)"
    );
    anyhow::ensure!(
        gas_cost_usdc.is_finite() && gas_cost_usdc >= 0.0,
        "gas_cost_usdc 数值无效"
    );
    Ok(())
}

/// EVM 卖出 token1 时必须获得的最低 USDC 输出。
pub fn minimum_buy_evm_output(
    hedge_size_token1: f64,
    worst_liquid_price: f64,
    liquid_fee_bps: f64,
    profit_buffer_bps: f64,
    gas_cost_usdc: f64,
) -> anyhow::Result<f64> {
    validate_economic_terms(
        hedge_size_token1,
        "hedge_size_token1",
        worst_liquid_price,
        liquid_fee_bps,
        profit_buffer_bps,
        gas_cost_usdc,
    )?;
    let hedge_cost = hedge_size_token1 * worst_liquid_price;
    Ok(hedge_cost * (1.0 + (liquid_fee_bps + profit_buffer_bps) / BPS) + gas_cost_usdc)
}

/// EVM 花费 USDC 买入 token1 时必须获得的最低 token1 输出。
pub fn minimum_sell_evm_output(
    evm_amount_in: f64,
    worst_liquid_price: f64,
    liquid_fee_bps: f64,
    profit_buffer_bps: f64,
    gas_cost_usdc: f64,
) -> anyhow::Result<f64> {
    validate_economic_terms(
        evm_amount_in,
        "evm_amount_in",
        worst_liquid_price,
        liquid_fee_bps,
        profit_buffer_bps,
        gas_cost_usdc,
    )?;
    let required_proceeds = evm_amount_in * (1.0 + profit_buffer_bps / BPS) + gas_cost_usdc;
    Ok(required_proceeds / (worst_liquid_price * (1.0 - liquid_fee_bps / BPS)))
}

#[derive(Debug, Clone, Copy)]
struct TradePlan {
    direction: ArbDirection,
    diff_bps: f64,
    minimum_diff_bps: f64,
    amount_in: f64,
    target_amount: f64,
    dex: DexName,
    enabled: bool,
    economically_safe: bool,
}

impl TradePlan {
    fn is_triggered(self) -> bool {
        self.enabled && self.economically_safe && self.diff_bps >= self.minimum_diff_bps
    }
}

/// 第二腿定价管线：最坏可成交价（含滑点偏移）→ 按市场精度归一化。
/// 机会评估与实际对冲下单共用这一条管线，避免两处定价规则漂移。
fn normalized_worst_liquid_price(
    direction: ArbDirection,
    order_book: &OrderBook,
    pair: &PairConfig,
    size: f64,
    size_decimals: u8,
) -> anyhow::Result<crate::liquid::client::NormalizedSpotOrder> {
    let is_buy = matches!(direction, ArbDirection::BuyDiff);
    let price = worst_case_liquid_price(
        direction,
        order_book.bid,
        order_book.ask,
        pair.limit_slippage,
    )?;
    normalize_spot_order(is_buy, price, size, size_decimals)
}

fn evaluate_opportunities(
    global: &Config,
    pair: &PairConfig,
    liquid_rules: &LiquidMarketRules,
    evm: &EvmPrice,
    order_book: &OrderBook,
) -> anyhow::Result<(TradePlan, TradePlan)> {
    let prices = [
        evm.sell_price,
        evm.buy_price,
        order_book.ask,
        order_book.bid,
    ];
    anyhow::ensure!(
        prices
            .into_iter()
            .all(|price| price.is_finite() && price > 0.0)
            && order_book.bid <= order_book.ask,
        "价格数据必须是正有限数且 bid <= ask"
    );

    let size = pair.order_size_token1;
    let buy_diff = (evm.sell_price - order_book.ask) / evm.sell_price * BPS;
    let sell_diff = (order_book.bid - evm.buy_price) / order_book.bid * BPS;
    let buy_worst_price = normalized_worst_liquid_price(
        ArbDirection::BuyDiff,
        order_book,
        pair,
        size,
        liquid_rules.size_decimals,
    )
    .context("buy_diff HL 最坏价格无效")?
    .price;
    let sell_worst_price = normalized_worst_liquid_price(
        ArbDirection::SellDiff,
        order_book,
        pair,
        size,
        liquid_rules.size_decimals,
    )
    .context("sell_diff HL 最坏价格无效")?
    .price;

    let sell_amount_in = size * evm.buy_price;
    let buy_target = minimum_buy_evm_output(
        size,
        buy_worst_price,
        global.liquid_fee_bps,
        global.profit_buffer_bps,
        global.gas_cost_usdc,
    )
    .context("buy_diff 经济下限计算失败")?;
    let sell_target = minimum_sell_evm_output(
        sell_amount_in,
        sell_worst_price,
        global.liquid_fee_bps,
        global.profit_buffer_bps,
        global.gas_cost_usdc,
    )
    .context("sell_diff 经济下限计算失败")?;

    let minimum_evm_sell_price = buy_target / size;
    let buy_economic_bps = (minimum_evm_sell_price - order_book.ask) / minimum_evm_sell_price * BPS;
    let maximum_evm_buy_price = (size * sell_worst_price * (1.0 - global.liquid_fee_bps / BPS)
        - global.gas_cost_usdc)
        / (size * (1.0 + global.profit_buffer_bps / BPS));
    let sell_economic_bps = (order_book.bid - maximum_evm_buy_price) / order_book.bid * BPS;

    Ok((
        TradePlan {
            direction: ArbDirection::BuyDiff,
            diff_bps: buy_diff,
            minimum_diff_bps: pair.ask_diff_percent.max(buy_economic_bps),
            amount_in: size,
            target_amount: buy_target,
            dex: evm.sell_dex,
            enabled: true,
            economically_safe: size * evm.sell_price >= buy_target,
        },
        TradePlan {
            direction: ArbDirection::SellDiff,
            diff_bps: sell_diff,
            minimum_diff_bps: pair.bid_diff_percent.max(sell_economic_bps),
            amount_in: sell_amount_in,
            target_amount: sell_target,
            dex: evm.buy_dex,
            enabled: pair.enable_sell_arb,
            economically_safe: size >= sell_target,
        },
    ))
}

/// 计算第二条腿需要对冲的 token1 数量。
pub fn hedge_size_token1(
    direction: ArbDirection,
    configured_size_token1: f64,
    evm_amount_out: Option<f64>,
) -> anyhow::Result<f64> {
    let size = match direction {
        ArbDirection::BuyDiff => configured_size_token1,
        ArbDirection::SellDiff => evm_amount_out
            .ok_or_else(|| anyhow::anyhow!("sell_diff 缺少 EVM amount_out，不能猜测对冲数量"))?,
    };
    anyhow::ensure!(size.is_finite() && size > 0.0, "对冲数量必须是正有限数");
    Ok(size)
}

/// 允许极小的浮点解析误差，但不把真实部分成交视为完整对冲。
pub fn fill_is_complete(expected: f64, actual: f64) -> bool {
    if !expected.is_finite() || !actual.is_finite() || expected <= 0.0 || actual < 0.0 {
        return false;
    }
    let tolerance = expected.abs() * 1e-9 + 1e-12;
    actual + tolerance >= expected
}

/// 单时间戳新鲜度：非未来时间且年龄不超过 max_age_ms
pub fn timestamp_is_fresh(timestamp: u64, now: u64, max_age_ms: u64) -> bool {
    timestamp <= now && now - timestamp <= max_age_ms
}

pub fn market_data_is_fresh(
    evm_received_at: u64,
    orderbook_received_at: u64,
    now: u64,
    max_age_ms: u64,
    max_skew_ms: u64,
) -> bool {
    timestamp_is_fresh(evm_received_at, now, max_age_ms)
        && timestamp_is_fresh(orderbook_received_at, now, max_age_ms)
        && evm_received_at.abs_diff(orderbook_received_at) <= max_skew_ms
}

/// 一个在途套利周期的全部状态：与周期同生共死，
/// 触发时整体创建、终态（完成/恢复）时整体清除，杜绝平行可空字段漂移。
struct InFlightCycle {
    dir: ArbDirection,
    started_at: Instant,
    /// HL 下单后期望的完整成交数量（下单前为 None）
    expected_liquid_size: Option<f64>,
    /// EVM 第一腿的实际 amount_out（TransferEventMissing 时为 None），
    /// 供 HL 拒单重试时重算对冲数量
    evm_amount_out: Option<f64>,
    /// 每个周期最多允许一次 HL 拒单重试
    liquid_retry_used: bool,
}

impl InFlightCycle {
    fn new(dir: ArbDirection) -> Self {
        Self {
            dir,
            started_at: Instant::now(),
            expected_liquid_size: None,
            evm_amount_out: None,
            liquid_retry_used: false,
        }
    }
}

pub struct ArbEngine {
    global: Arc<Config>,
    pair: Arc<PairConfig>,
    state: ArbState,
    /// Some ⟺ 存在在途周期（EvmSwapping/LiquidOrdering）
    cycle: Option<InFlightCycle>,
    last_evm_price: Option<EvmPrice>,
    last_order_book: Option<OrderBook>,
    liquid_rules: LiquidMarketRules,
    recovery_record: Option<RecoveryRecord>,
    /// 恢复记录专用长连接：避免每次持久化重开连接、重跑 DDL 并加剧写锁竞争
    recovery_conn: rusqlite::Connection,

    evm_rx: broadcast::Receiver<EvmPrice>,
    ob_rx: broadcast::Receiver<OrderBook>,
    evm_tx: mpsc::Sender<EvmSwapRequest>,
    liquid_tx: mpsc::Sender<LiquidCommand>,
    result_rx: mpsc::Receiver<TradeResult>,
    db_tx: broadcast::Sender<PriceRecord>,
}

impl ArbEngine {
    // Actor 的输入输出使用显式参数，构造依赖保持可见。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        global: Arc<Config>,
        pair: Arc<PairConfig>,
        evm_rx: broadcast::Receiver<EvmPrice>,
        ob_rx: broadcast::Receiver<OrderBook>,
        evm_tx: mpsc::Sender<EvmSwapRequest>,
        liquid_tx: mpsc::Sender<LiquidCommand>,
        result_rx: mpsc::Receiver<TradeResult>,
        db_tx: broadcast::Sender<PriceRecord>,
        liquid_rules: LiquidMarketRules,
    ) -> anyhow::Result<Self> {
        let recovery_conn = db::open_initialized(&global.db_path)?;
        Ok(Self {
            global,
            pair,
            state: ArbState::Idle,
            cycle: None,
            last_evm_price: None,
            last_order_book: None,
            liquid_rules,
            recovery_record: None,
            recovery_conn,
            evm_rx,
            ob_rx,
            evm_tx,
            liquid_tx,
            result_rx,
            db_tx,
        })
    }

    pub async fn run(mut self, shutdown: CancellationToken) {
        let mut stopping = shutdown.is_cancelled();
        loop {
            if stopping && matches!(self.state, ArbState::Idle | ArbState::RecoveryRequired) {
                break;
            }
            // 注意：evm_rx/ob_rx 在 stopping 期间保持消费——在途第一腿确认后仍需要
            // 新鲜订单簿完成对冲（OrderBookWatcher 会等所有引擎退出后才停止推送）。
            // 不接新机会由循环顶部的 Idle 退出条件保证。
            tokio::select! {
                result = self.evm_rx.recv() => match result {
                    Ok(price) if price.pair_id == self.pair.symbol => {
                        self.last_evm_price = Some(price);
                        self.try_arb().await;
                    }
                    Ok(_) => {}  // 其他 pair 的价格，忽略
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(n, "[ArbEngine] evm_rx 滞后，跳过 {n} 条价格，清空旧价格");
                        self.last_evm_price = None;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        error!("[ArbEngine] evm_rx 已关闭，退出");
                        break;
                    }
                },
                result = self.ob_rx.recv() => match result {
                    Ok(ob) if ob.symbol == self.pair.symbol => {
                        if ob.created_at > 0 {
                            let latency_ms = ob.received_at.saturating_sub(ob.created_at) as f64;
                            crate::metrics::record_hl_orderbook_latency(&ob.symbol, latency_ms);
                        }
                        self.last_order_book = Some(ob);
                        self.try_arb().await;
                    }
                    Ok(_) => {}  // 其他 pair 的订单簿，忽略
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(n, "[ArbEngine] ob_rx 滞后，跳过 {n} 条订单簿，清空旧订单簿");
                        self.last_order_book = None;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        error!("[ArbEngine] ob_rx 已关闭，退出");
                        break;
                    }
                },
                result = self.result_rx.recv() => match result {
                    Some(result) => self.handle_trade_result(result).await,
                    None => {
                        self.require_recovery("trade result channel 已关闭");
                        break;
                    }
                },
                _ = shutdown.cancelled(), if !stopping => {
                    stopping = true;
                    info!(symbol = %self.pair.symbol, state = ?self.state, "[ArbEngine] 停止接收新机会，等待在途交易完成");
                },
            }
        }
        info!(symbol = %self.pair.symbol, "[ArbEngine] 已退出");
    }

    async fn try_arb(&mut self) {
        if !self.state.can_start_quote() {
            return;
        }

        let (evm, order_book) = match (&self.last_evm_price, &self.last_order_book) {
            (Some(evm), Some(order_book)) => (evm, order_book),
            _ => return,
        };
        if !market_data_is_fresh(
            evm.received_at,
            order_book.received_at,
            unix_timestamp_ms(),
            MAX_MARKET_AGE_MS,
            MAX_MARKET_SKEW_MS,
        ) {
            warn!(
                symbol = %self.pair.symbol,
                evm_received_at = evm.received_at,
                orderbook_received_at = order_book.received_at,
                "[ArbEngine] 市场数据过期或时间偏差过大，跳过"
            );
            return;
        }

        let (buy_plan, sell_plan) = match evaluate_opportunities(
            &self.global,
            &self.pair,
            &self.liquid_rules,
            evm,
            order_book,
        ) {
            Ok(plans) => plans,
            Err(error) => {
                warn!(error = %error, symbol = %self.pair.symbol, "[ArbEngine] 套利机会计算失败，跳过");
                return;
            }
        };

        info!(
            buy_diff = buy_plan.diff_bps,
            sell_diff = sell_plan.diff_bps,
            min_bps = buy_plan.minimum_diff_bps,
            sell_min_bps = sell_plan.minimum_diff_bps,
            sell_dex = %buy_plan.dex,
            buy_dex = %sell_plan.dex,
            symbol = %self.pair.symbol,
            "[ArbEngine] 检测差价"
        );

        crate::metrics::record_price_diff(
            &self.pair.symbol,
            buy_plan.diff_bps,
            sell_plan.diff_bps,
            buy_plan.minimum_diff_bps,
        );
        crate::metrics::record_hl_price(&self.pair.symbol, order_book.ask, order_book.bid);

        if self
            .db_tx
            .send(PriceRecord {
                pair: self.pair.symbol.clone(),
                evm_buy_price: evm.buy_price,
                evm_sell_price: evm.sell_price,
                liquid_ask: order_book.ask,
                liquid_bid: order_book.bid,
                sell_diff: sell_plan.diff_bps,
                buy_diff: buy_plan.diff_bps,
            })
            .is_err()
        {
            warn!("[ArbEngine] db_tx 已关闭，价格记录丢失");
        }

        // dry_run 记录所有满足条件的方向；实盘只执行第一个触发的方向
        for plan in [buy_plan, sell_plan]
            .into_iter()
            .filter(|plan| plan.is_triggered())
        {
            if self.global.dry_run {
                info!(
                    direction = plan.direction.label(),
                    diff_bps = plan.diff_bps,
                    minimum_diff_bps = plan.minimum_diff_bps,
                    dex = %plan.dex,
                    symbol = %self.pair.symbol,
                    "[ArbEngine][DRY_RUN] 套利条件满足，但不执行"
                );
                crate::metrics::record_arb_dry_run_triggered(
                    &self.pair.symbol,
                    plan.direction.label(),
                );
                continue;
            }
            info!(
                direction = plan.direction.label(),
                diff_bps = plan.diff_bps,
                minimum_diff_bps = plan.minimum_diff_bps,
                amount_in = plan.amount_in,
                target_amount = plan.target_amount,
                dex = %plan.dex,
                "[ArbEngine] 触发套利"
            );
            self.send_evm_swap(plan.direction, plan.amount_in, plan.target_amount, plan.dex)
                .await;
            return;
        }
    }

    async fn send_evm_swap(
        &mut self,
        dir: ArbDirection,
        amount_in: f64,
        target_amount: f64,
        dex: DexName,
    ) {
        // is_buy 从 dir 推导，消除调用方传入冗余 bool 的风险
        let is_buy = matches!(dir, ArbDirection::SellDiff);
        crate::metrics::record_arb_triggered(&self.pair.symbol, dir.label());
        self.cycle = Some(InFlightCycle::new(dir));
        if let Err(error) = self.persist_stage("evm_pending", "等待 EVM 执行", None, None) {
            self.require_recovery(&format!("无法持久化 EVM 待执行状态: {error}"));
            return;
        }
        if let Err(e) = self
            .evm_tx
            .send(EvmSwapRequest {
                is_buy,
                amount_in,
                target_amount,
                dex,
            })
            .await
        {
            error!(error = %e, "[ArbEngine] 发送 EvmSwap 指令失败");
            self.finish_arb_cycle();
        } else {
            self.state = ArbState::EvmSwapping;
        }
    }

    fn persist_stage(
        &mut self,
        stage: &str,
        reason: &str,
        evm_tx_hash: Option<String>,
        liquid_oid: Option<u64>,
    ) -> anyhow::Result<()> {
        let direction = self
            .cycle
            .as_ref()
            .map(|cycle| cycle.dir.label())
            .unwrap_or("unknown")
            .to_owned();
        let prev = self.recovery_record.as_ref();
        let record = RecoveryRecord {
            pair: self.pair.symbol.clone(),
            stage: stage.to_owned(),
            direction,
            evm_tx_hash: evm_tx_hash.or_else(|| prev.and_then(|r| r.evm_tx_hash.clone())),
            liquid_oid: liquid_oid.or_else(|| prev.and_then(|r| r.liquid_oid)),
            // reason 可能内嵌 RPC 错误（含带凭据的 URL），持久化边界统一脱敏
            reason: self.global.redact_rpc(reason),
        };
        // busy_timeout 下同步 SQLite 写最长可阻塞 5s，避免钉死 tokio worker
        tokio::task::block_in_place(|| db::upsert_recovery(&self.recovery_conn, &record))?;
        self.recovery_record = Some(record);
        Ok(())
    }

    fn clear_persisted_cycle(&mut self) -> anyhow::Result<()> {
        tokio::task::block_in_place(|| db::clear_recovery(&self.recovery_conn, &self.pair.symbol))?;
        self.recovery_record = None;
        Ok(())
    }

    /// 上报本周期耗时（每个周期只在终态调用一次；cycle 随后被清除，不会重复上报）
    fn record_cycle_duration(&self) {
        if let Some(cycle) = &self.cycle {
            crate::metrics::record_arb_execution_duration(
                &self.pair.symbol,
                cycle.started_at.elapsed().as_secs_f64(),
            );
        }
    }

    /// 记录套利耗时并复位状态机到 Idle
    fn finish_arb_cycle(&mut self) {
        if let Err(error) = self.clear_persisted_cycle() {
            self.require_recovery(&format!("清除已完成交易的恢复记录失败: {error}"));
            return;
        }
        self.record_cycle_duration();
        self.state = ArbState::Idle;
        self.cycle = None;
    }

    fn require_recovery(&mut self, reason: &str) {
        self.record_cycle_duration();
        crate::metrics::record_arb_recovery_required(&self.pair.symbol);
        self.state = ArbState::RecoveryRequired;
        // persist_stage 从 cycle 读取方向，须在清除 cycle 之前调用
        if let Err(error) = self.persist_stage("recovery_required", reason, None, None) {
            error!(error = %error, "[ArbEngine] RecoveryRequired 持久化失败");
        }
        self.cycle = None;
        error!(
            symbol = %self.pair.symbol,
            reason,
            "[ArbEngine] 对冲未完成，进入 RecoveryRequired，停止新套利"
        );
    }

    /// 从在途周期和当前 orderbook 推导 HL 限价单方向，发出 LiquidOrder。
    /// 对冲数量来源于 cycle.evm_amount_out（EVM 第一腿实际产出）。
    async fn proceed_to_liquid_order(&mut self, context: &str) {
        let (dir, evm_amount_out) = match &self.cycle {
            Some(cycle) => (cycle.dir, cycle.evm_amount_out),
            None => {
                // EVM 第一腿已成功、周期状态却丢失——状态机异常且资金已划转，
                // 必须人工介入而非静默复位
                self.require_recovery(&format!("{context} 下无在途周期，状态机异常"));
                return;
            }
        };

        let Some(ob) = self.last_order_book.as_ref() else {
            self.require_recovery(&format!("{context}: order_book 为空"));
            return;
        };
        if !timestamp_is_fresh(ob.received_at, unix_timestamp_ms(), MAX_MARKET_AGE_MS) {
            self.require_recovery(&format!("{context}: order_book 已过期"));
            return;
        }

        // buy_diff: 已在 EVM 卖出 token1，现在在 HL 买入补仓
        // sell_diff: 已在 EVM 买入 token1，现在在 HL 卖出套现
        let is_buy = matches!(dir, ArbDirection::BuyDiff);
        let size = match hedge_size_token1(dir, self.pair.order_size_token1, evm_amount_out) {
            Ok(size) => size,
            Err(error) => {
                self.require_recovery(&format!("{context}: {error}"));
                return;
            }
        };
        let normalized = match normalized_worst_liquid_price(
            dir,
            ob,
            &self.pair,
            size,
            self.liquid_rules.size_decimals,
        ) {
            Ok(order) => order,
            Err(error) => {
                self.require_recovery(&format!("{context}: {error}"));
                return;
            }
        };
        let price = normalized.price;
        let size = normalized.size;

        if let Err(error) = self.persist_stage("liquid_pending", context, None, None) {
            error!(error = %error, "[ArbEngine] Liquid 待执行状态持久化失败，继续优先完成对冲");
        }
        self.state = ArbState::LiquidOrdering;
        if let Some(cycle) = self.cycle.as_mut() {
            cycle.expected_liquid_size = Some(size);
        }
        if let Err(e) = self
            .liquid_tx
            .send(LiquidCommand::Order {
                is_buy,
                price,
                size,
            })
            .await
        {
            error!(error = %e, "[ArbEngine] 发送 LiquidOrder 失败");
            self.require_recovery("发送 LiquidOrder 失败");
        }
    }

    async fn handle_trade_result(&mut self, result: TradeResult) {
        match (&self.state, &result) {
            (
                ArbState::EvmSwapping,
                TradeResult::EvmSwapSuccess {
                    amount_out,
                    tx_hash,
                },
            ) => {
                info!(
                    amount_out,
                    tx_hash, "[ArbEngine] EVM swap 成功，发起 Liquid 下单"
                );
                if let Err(error) = self.persist_stage(
                    "evm_confirmed",
                    "EVM 已确认，等待 HL 对冲",
                    Some(tx_hash.clone()),
                    None,
                ) {
                    error!(error = %error, "[ArbEngine] EVM 确认状态持久化失败，继续优先完成对冲");
                }
                if let Some(cycle) = self.cycle.as_mut() {
                    cycle.evm_amount_out = Some(*amount_out);
                }
                self.proceed_to_liquid_order("EvmSwapSuccess").await;
                return;
            }
            (ArbState::EvmSwapping, TradeResult::EvmSwapAborted { reason }) => {
                // 发送 tx 之前的安全放弃：链上零资金变动，直接复位等下一轮
                warn!(reason, "[ArbEngine] EVM 发送前安全放弃本轮");
            }
            (ArbState::EvmSwapping, TradeResult::EvmSwapConfirmedAmountUnknown { tx_hash }) => {
                // 链上已成功但 Transfer 事件缺失：资金已划转，不可重试
                error!(
                    tx_hash,
                    "[ArbEngine] EVM swap 链上完成但金额未知，继续 Liquid 下单"
                );
                if let Err(error) = self.persist_stage(
                    "evm_confirmed_amount_unknown",
                    "EVM 已确认但实际金额未知",
                    Some(tx_hash.clone()),
                    None,
                ) {
                    error!(error = %error, "[ArbEngine] 金额未知状态持久化失败，继续优先完成对冲");
                }
                // cycle.evm_amount_out 保持 None：金额未知，SellDiff 会在
                // hedge_size_token1 处拒绝猜测并进入恢复
                self.proceed_to_liquid_order("TransferEventMissing").await;
                return;
            }
            (ArbState::EvmSwapping, TradeResult::EvmSwapFailed { reason }) => {
                // 发送阶段的失败（approve/swap tx 回滚、回执错误）：状态不明，fail-closed
                self.require_recovery(reason);
                return;
            }
            (ArbState::EvmSwapping, TradeResult::EvmSwapUnknown { reason }) => {
                self.require_recovery(reason);
                return;
            }
            (
                ArbState::LiquidOrdering,
                TradeResult::LiquidFilled {
                    oid,
                    avg_price,
                    total_size,
                },
            ) => {
                info!(
                    oid,
                    avg_price, total_size, "[ArbEngine] Liquid 成交，套利完成"
                );
                if let Err(error) =
                    self.persist_stage("liquid_filled", "HL 对冲已成交", None, Some(*oid))
                {
                    error!(error = %error, "[ArbEngine] HL 成交状态持久化失败");
                }
                let Some(expected) = self.cycle.as_ref().and_then(|c| c.expected_liquid_size)
                else {
                    // 不能用 0.0 兜底：那会把任意成交量误判为完整对冲
                    self.require_recovery("expected_liquid_size 缺失，状态机异常");
                    return;
                };
                if !fill_is_complete(expected, *total_size) {
                    self.require_recovery("Liquid 仅部分成交");
                    return;
                }
            }
            (ArbState::LiquidOrdering, TradeResult::LiquidResting { oid }) => {
                warn!(oid, "[ArbEngine] Liquid 挂单未立即成交，发起取消");
                if let Err(error) = self.persist_stage(
                    "liquid_resting",
                    "HL IOC 返回 resting，正在撤单",
                    None,
                    Some(*oid),
                ) {
                    error!(error = %error, "[ArbEngine] resting 订单状态持久化失败");
                }
                match self
                    .liquid_tx
                    .send(LiquidCommand::Cancel { oid: *oid })
                    .await
                {
                    Ok(_) => return, // 留在 LiquidOrdering，等待撤单响应后再复位
                    Err(e) => {
                        error!(error = %e, "[ArbEngine] 发送 LiquidCancel 失败");
                        self.require_recovery("发送 LiquidCancel 失败");
                        return;
                    }
                }
            }
            (ArbState::LiquidOrdering, TradeResult::LiquidCancelled { oid }) => {
                warn!(oid, "[ArbEngine] Liquid 挂单已撤销但对冲未完成");
                self.require_recovery("Liquid 挂单撤销后仍有未对冲仓位");
                return;
            }
            (ArbState::LiquidOrdering, TradeResult::LiquidFailed { reason, retriable }) => {
                // 仅交易所明确拒单（订单从未上簿）允许用最新订单簿重试一次；
                // 传输错误/超时下订单状态不明，重试会有双重对冲风险
                let retry_available = self
                    .cycle
                    .as_ref()
                    .is_some_and(|cycle| !cycle.liquid_retry_used);
                if *retriable && retry_available {
                    if let Some(cycle) = self.cycle.as_mut() {
                        cycle.liquid_retry_used = true;
                    }
                    warn!(reason, "[ArbEngine] HL 明确拒单，用最新订单簿重试一次");
                    self.proceed_to_liquid_order("LiquidRetry").await;
                    return;
                }
                error!(reason, "[ArbEngine] Liquid 下单失败");
                self.require_recovery(reason);
                return;
            }
            _ => {
                warn!(
                    state = ?self.state,
                    result = ?result,
                    "[ArbEngine] 收到与当前状态不匹配的 TradeResult，忽略"
                );
                return;
            }
        }
        self.finish_arb_cycle();
    }
}
