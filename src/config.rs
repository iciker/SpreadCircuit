use std::{collections::HashSet, env, str::FromStr};

use alloy::primitives::Address;
use anyhow::{bail, ensure, Context, Result};

const LIVE_TRADING_ACK: &str = "I_UNDERSTAND_THE_RISK";
pub const HYPEREVM_USDC_ADDRESS: &str = "0xb8ce59fc3717ada4c02eadf9682a9e934f625ebb";

/// 配置来源抽象。生产环境使用进程环境变量，其他调用方也可以提供受控配置源。
pub trait ConfigSource {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct ProcessEnv;

impl ConfigSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        env::var(key).ok()
    }
}

#[derive(Clone)]
pub struct Config {
    pub private_key: String,
    pub https_rpc: String,
    pub wss_rpc: String,
    pub wss_api: String,
    pub min_out_bps: f64,
    pub limit_slippage: f64,
    pub liquid_fee_bps: f64,
    /// 单次链上交易的预估 Gas 成本，按 token0（当前配置为 USDC）计价。
    pub gas_cost_usdc: f64,
    pub profit_buffer_bps: f64,
    pub hyperswap_router01: String,
    pub hyperswap_quotev2: String,
    pub prjx_quotev2: String,
    pub prjx_routerv2: String,
    pub kitten_quoter: String,
    pub kitten_router: String,
    pub dry_run: bool,
    pub metrics_port: u16,
    pub metrics_host: String,
    pub log_dir: String,
    pub db_path: String,
    pub db_max_price_rows: usize,
    pairs: Vec<PairConfig>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("private_key", &"***")
            .field("https_rpc", &redact_url(&self.https_rpc))
            .field("dry_run", &self.dry_run)
            .field("log_dir", &self.log_dir)
            .field("pairs", &self.pairs)
            .finish()
    }
}

impl Config {
    pub fn try_from_env() -> Result<Self> {
        Self::try_from_source(&ProcessEnv)
    }

    pub fn try_from_source(source: &impl ConfigSource) -> Result<Self> {
        let dry_run = parse_bool_or(source, "DRY_RUN", true)?;
        if !dry_run {
            let ack = source.get("LIVE_TRADING_ACK").unwrap_or_default();
            ensure!(
                ack == LIVE_TRADING_ACK,
                "DRY_RUN=false 时必须设置 LIVE_TRADING_ACK={LIVE_TRADING_ACK}"
            );
        }

        let mut config = Self {
            private_key: required(source, "PRIVATE_KEY")?,
            https_rpc: required(source, "HTTPS_RPC")?,
            wss_rpc: required(source, "WSS_RPC")?,
            wss_api: required(source, "WSS_API")?,
            min_out_bps: parse_or(source, "MIN_OUT_BPS", 30.0)?,
            limit_slippage: parse_or(source, "LIMIT_SLIPPAGE", 21.0)?,
            liquid_fee_bps: parse_or(source, "LIQUID_FEE_BPS", 1.0)?,
            gas_cost_usdc: parse_or(source, "GAS_COST_USDC", 0.02)?,
            profit_buffer_bps: parse_or(source, "PROFIT_BUFFER_BPS", 12.0)?,
            hyperswap_router01: required(source, "HYPERSWAP_ROUTER01")?,
            hyperswap_quotev2: required(source, "HYPERSWAP_QUOTEV2")?,
            prjx_quotev2: required(source, "PRJX_QUOTEV2")?,
            prjx_routerv2: required(source, "PRJX_ROUTERV2")?,
            kitten_quoter: string_or(source, "KITTEN_QUOTER", ""),
            kitten_router: string_or(source, "KITTEN_ROUTER", ""),
            dry_run,
            metrics_port: parse_or(source, "METRICS_PORT", 9090)?,
            metrics_host: string_or(source, "METRICS_HOST", "127.0.0.1"),
            log_dir: string_or(source, "LOG_DIR", "logs"),
            db_path: string_or(source, "DB_PATH", "data.db"),
            db_max_price_rows: parse_or(source, "DB_MAX_PRICE_ROWS", 1_000_000)?,
            pairs: Vec::new(),
        };
        config.validate_global()?;

        let prefixes = required(source, "PAIRS")?
            .split(',')
            .map(|value| value.trim().to_uppercase())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        ensure!(!prefixes.is_empty(), "PAIRS 至少需要一个交易对");

        let mut symbols = HashSet::new();
        for prefix in prefixes {
            let pair = PairConfig::try_from_source_prefix(source, &prefix, &config)?;
            ensure!(
                symbols.insert(pair.symbol.clone()),
                "交易对 symbol 重复: {}",
                pair.symbol
            );
            config.pairs.push(pair);
        }

        Ok(config)
    }

    pub fn pairs(&self) -> Vec<PairConfig> {
        self.pairs.clone()
    }

    fn validate_global(&self) -> Result<()> {
        ensure!(!self.private_key.trim().is_empty(), "PRIVATE_KEY 不能为空");
        validate_bps("MIN_OUT_BPS", self.min_out_bps, false)?;
        validate_bps("LIMIT_SLIPPAGE", self.limit_slippage, false)?;
        validate_bps("LIQUID_FEE_BPS", self.liquid_fee_bps, false)?;
        validate_bps("PROFIT_BUFFER_BPS", self.profit_buffer_bps, false)?;
        ensure!(
            self.gas_cost_usdc.is_finite() && self.gas_cost_usdc >= 0.0,
            "GAS_COST_USDC 必须是非负有限数"
        );
        ensure!(self.db_max_price_rows > 0, "DB_MAX_PRICE_ROWS 必须大于 0");
        validate_address("HYPERSWAP_ROUTER01", &self.hyperswap_router01)?;
        validate_address("HYPERSWAP_QUOTEV2", &self.hyperswap_quotev2)?;
        validate_address("PRJX_QUOTEV2", &self.prjx_quotev2)?;
        validate_address("PRJX_ROUTERV2", &self.prjx_routerv2)?;
        validate_optional_address("KITTEN_QUOTER", &self.kitten_quoter)?;
        validate_optional_address("KITTEN_ROUTER", &self.kitten_router)?;
        ensure!(
            self.kitten_quoter.is_empty() == self.kitten_router.is_empty(),
            "KITTEN_QUOTER 与 KITTEN_ROUTER 必须同时配置或同时留空"
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct PairConfig {
    pub token0: String,
    pub decimals0: u8,
    pub token1: String,
    pub decimals1: u8,
    pub symbol: String,
    pub spot_coin: String,
    /// 每次交易的 token1 数量，不是 USDC 名义金额。
    pub order_size_token1: f64,
    pub ask_diff_percent: f64,
    pub bid_diff_percent: f64,
    pub min_out_bps: f64,
    pub limit_slippage: f64,
    pub fee_tier: u32,
    pub enable_sell_arb: bool,
    pub use_hyperswap: bool,
    pub use_kitten: bool,
}

impl PairConfig {
    pub fn try_from_source_prefix(
        source: &impl ConfigSource,
        prefix: &str,
        global: &Config,
    ) -> Result<Self> {
        let p = prefix.to_uppercase();
        let pair = Self {
            token0: required(source, &format!("{p}_TOKEN0"))?,
            decimals0: parse_required(source, &format!("{p}_DECIMALS0"))?,
            token1: required(source, &format!("{p}_TOKEN1"))?,
            decimals1: parse_required(source, &format!("{p}_DECIMALS1"))?,
            symbol: string_or(source, &format!("{p}_SYMBOL"), prefix),
            spot_coin: required(source, &format!("{p}_SPOT_COIN"))?,
            order_size_token1: parse_required(source, &format!("{p}_ORDER_AMOUNT"))?,
            ask_diff_percent: parse_required(source, &format!("{p}_ASK_DIFF_PERCENT"))?,
            bid_diff_percent: parse_required(source, &format!("{p}_BID_DIFF_PERCENT"))?,
            min_out_bps: parse_or(source, &format!("{p}_MIN_OUT_BPS"), global.min_out_bps)?,
            limit_slippage: parse_or(
                source,
                &format!("{p}_LIMIT_SLIPPAGE"),
                global.limit_slippage,
            )?,
            fee_tier: parse_required(source, &format!("{p}_FEE_TIER"))?,
            enable_sell_arb: parse_bool_or(source, &format!("{p}_ENABLE_SELL_ARB"), false)?,
            use_hyperswap: parse_bool_or(source, &format!("{p}_USE_HYPERSWAP"), true)?,
            use_kitten: parse_bool_or(source, &format!("{p}_USE_KITTEN"), true)?,
        };
        pair.validate(&p)?;
        Ok(pair)
    }

    fn validate(&self, prefix: &str) -> Result<()> {
        validate_address(&format!("{prefix}_TOKEN0"), &self.token0)?;
        validate_address(&format!("{prefix}_TOKEN1"), &self.token1)?;
        let token0 = self.token0.parse::<Address>()?;
        let token1 = self.token1.parse::<Address>()?;
        ensure!(token0 != token1, "{prefix}_TOKEN0 与 TOKEN1 不能相同");
        let expected_usdc = HYPEREVM_USDC_ADDRESS.parse::<Address>()?;
        ensure!(
            token0 == expected_usdc,
            "{prefix}_TOKEN0 必须是 HyperEVM USDC ({HYPEREVM_USDC_ADDRESS})"
        );
        ensure!(
            self.decimals0 == 6,
            "{prefix}_DECIMALS0 必须为 HyperEVM USDC 的 6 位精度"
        );
        ensure!(self.decimals0 <= 38, "{prefix}_DECIMALS0 不能超过 38");
        ensure!(self.decimals1 <= 38, "{prefix}_DECIMALS1 不能超过 38");
        ensure!(!self.symbol.trim().is_empty(), "{prefix}_SYMBOL 不能为空");
        ensure!(
            !self.spot_coin.trim().is_empty(),
            "{prefix}_SPOT_COIN 不能为空"
        );
        ensure!(
            self.order_size_token1.is_finite()
                && self.order_size_token1 > 0.0
                && self.order_size_token1 < 1e15,
            "{prefix}_ORDER_AMOUNT 必须是正有限数且小于 1e15；该值表示 token1 数量"
        );
        validate_bps(
            &format!("{prefix}_ASK_DIFF_PERCENT"),
            self.ask_diff_percent,
            false,
        )?;
        validate_bps(
            &format!("{prefix}_BID_DIFF_PERCENT"),
            self.bid_diff_percent,
            true,
        )?;
        validate_bps(&format!("{prefix}_MIN_OUT_BPS"), self.min_out_bps, false)?;
        validate_bps(
            &format!("{prefix}_LIMIT_SLIPPAGE"),
            self.limit_slippage,
            false,
        )?;
        ensure!(
            self.fee_tier <= 0x00ff_ffff,
            "{prefix}_FEE_TIER 超出 uint24 范围"
        );
        Ok(())
    }
}

fn required(source: &impl ConfigSource, key: &str) -> Result<String> {
    let value = source
        .get(key)
        .with_context(|| format!("缺少环境变量 {key}"))?;
    ensure!(!value.trim().is_empty(), "环境变量 {key} 不能为空");
    Ok(value)
}

fn parse_required<T>(source: &impl ConfigSource, key: &str) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let value = required(source, key)?;
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("{key} 值无效 '{value}': {error}"))
}

fn parse_or<T>(source: &impl ConfigSource, key: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    match source.get(key) {
        Some(value) => value
            .parse()
            .map_err(|error| anyhow::anyhow!("{key} 值无效 '{value}': {error}")),
        None => Ok(default),
    }
}

fn parse_bool_or(source: &impl ConfigSource, key: &str, default: bool) -> Result<bool> {
    let Some(value) = source.get(key) else {
        return Ok(default);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("{key} 只接受 true 或 false，收到 '{value}'"),
    }
}

fn string_or(source: &impl ConfigSource, key: &str, default: &str) -> String {
    source.get(key).unwrap_or_else(|| default.into())
}

fn validate_bps(key: &str, value: f64, allow_negative: bool) -> Result<()> {
    ensure!(value.is_finite(), "{key} 必须是有限数");
    let min = if allow_negative { -10_000.0 } else { 0.0 };
    ensure!(
        value >= min && value < 10_000.0,
        "{key} 必须位于 [{min}, 10000) bps"
    );
    Ok(())
}

fn validate_address(key: &str, value: &str) -> Result<()> {
    let address = value
        .parse::<Address>()
        .map_err(|error| anyhow::anyhow!("{key} 地址无效: {error}"))?;
    ensure!(address != Address::ZERO, "{key} 不能是零地址");
    Ok(())
}

fn validate_optional_address(key: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        Ok(())
    } else {
        validate_address(key, value)
    }
}

fn redact_url(value: &str) -> String {
    let Some((scheme, remainder)) = value.split_once("://") else {
        return "***".into();
    };
    let authority = remainder.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        "***".into()
    } else {
        format!("{scheme}://{authority}/***")
    }
}
