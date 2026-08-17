use anyhow::{anyhow, Result};
use hyperliquid_rust_sdk::{
    BaseUrl, ClientCancelRequest, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient,
    ExchangeDataStatus, ExchangeResponseStatus, InfoClient,
};
use tracing::{error, info, warn};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedSpotOrder {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidMarketRules {
    pub asset: String,
    pub base_token: String,
    pub quote_token: String,
    pub size_decimals: u8,
}

pub fn validate_market_identity(
    symbol: &str,
    configured_coin: &str,
    canonical_asset: &str,
    base_token: &str,
    quote_token: &str,
) -> Result<()> {
    anyhow::ensure!(
        base_token.eq_ignore_ascii_case(symbol),
        "HL base token {base_token} 与配置 symbol {symbol} 不一致"
    );
    anyhow::ensure!(
        quote_token.eq_ignore_ascii_case("USDC"),
        "HL quote token 必须是 USDC，收到 {quote_token}"
    );
    let pair_alias = format!("{base_token}/{quote_token}");
    anyhow::ensure!(
        configured_coin.eq_ignore_ascii_case(canonical_asset)
            || configured_coin.eq_ignore_ascii_case(base_token)
            || configured_coin.eq_ignore_ascii_case(&pair_alias),
        "SPOT_COIN {configured_coin} 不对应 HL 市场 {canonical_asset}"
    );
    Ok(())
}

fn decimal_places_for_significant_figures(value: f64, significant_figures: i32) -> u32 {
    let magnitude = value.abs().log10().floor() as i32;
    significant_figures
        .saturating_sub(1)
        .saturating_sub(magnitude)
        .max(0) as u32
}

pub fn normalize_spot_order(
    is_buy: bool,
    price: f64,
    size: f64,
    size_decimals: u8,
) -> Result<NormalizedSpotOrder> {
    anyhow::ensure!(price.is_finite() && price > 0.0, "price 必须为正有限数");
    anyhow::ensure!(size.is_finite() && size > 0.0, "size 必须为正有限数");
    anyhow::ensure!(size_decimals <= 8, "HL spot size decimals 不能超过 8");

    let price_decimals =
        (8_u32 - u32::from(size_decimals)).min(decimal_places_for_significant_figures(price, 5));
    let price_factor = 10_f64.powi(price_decimals as i32);
    let epsilon = 1e-10;
    let normalized_price = if is_buy {
        (price * price_factor - epsilon).ceil() / price_factor
    } else {
        (price * price_factor + epsilon).floor() / price_factor
    };
    let size_factor = 10_f64.powi(i32::from(size_decimals));
    let normalized_size = (size * size_factor + epsilon).floor() / size_factor;
    anyhow::ensure!(normalized_size > 0.0, "size 按市场精度截断后为 0");

    Ok(NormalizedSpotOrder {
        price: normalized_price,
        size: normalized_size,
    })
}

pub fn validate_configured_size(size: f64, size_decimals: u8) -> Result<()> {
    let normalized = normalize_spot_order(true, 1.0, size, size_decimals)?.size;
    let tolerance = size.abs() * 1e-12 + 1e-12;
    anyhow::ensure!(
        (normalized - size).abs() <= tolerance,
        "ORDER_AMOUNT={size} 无法用 HL szDecimals={size_decimals} 精确表示"
    );
    Ok(())
}

pub struct LiquidClient {
    exchange: ExchangeClient,
    coin: String,
    rules: LiquidMarketRules,
}

#[derive(Debug)]
pub struct OrderResult {
    pub status: OrderStatus,
    pub oid: Option<u64>,
    pub avg_price: Option<f64>,
    pub total_size: Option<f64>,
}

impl OrderResult {
    fn with_status(status: OrderStatus) -> Self {
        Self {
            status,
            oid: None,
            avg_price: None,
            total_size: None,
        }
    }
}

#[derive(Debug)]
pub enum OrderStatus {
    Filled,
    Resting,
    /// 交易所明确拒单：订单从未上簿，调用方可安全重试
    Rejected,
    /// 状态不明（响应缺 data / 未识别 status）：禁止重试
    Failed,
    Unknown,
}

impl LiquidClient {
    pub async fn new(private_key: &str, coin: &str, symbol: &str) -> Result<Self> {
        let wallet = private_key
            .parse::<ethers::signers::LocalWallet>()
            .map_err(|e| anyhow!("Invalid private key: {e}"))?;

        let info = InfoClient::new(None, Some(BaseUrl::Mainnet))
            .await
            .map_err(|e| anyhow!("Failed to create InfoClient: {e}"))?;
        let spot_meta = info
            .spot_meta()
            .await
            .map_err(|e| anyhow!("Failed to load spot metadata: {e}"))?;
        let mut matches = Vec::new();
        for market in &spot_meta.universe {
            let Some(base) = spot_meta
                .tokens
                .iter()
                .find(|token| token.index == market.tokens[0])
            else {
                continue;
            };
            let Some(quote) = spot_meta
                .tokens
                .iter()
                .find(|token| token.index == market.tokens[1])
            else {
                continue;
            };
            let pair_alias = format!("{}/{}", base.name, quote.name);
            if quote.name.eq_ignore_ascii_case("USDC")
                && (coin.eq_ignore_ascii_case(&market.name)
                    || coin.eq_ignore_ascii_case(&base.name)
                    || coin.eq_ignore_ascii_case(&pair_alias))
            {
                matches.push(LiquidMarketRules {
                    asset: market.name.clone(),
                    base_token: base.name.clone(),
                    quote_token: quote.name.clone(),
                    size_decimals: base.sz_decimals,
                });
            }
        }
        anyhow::ensure!(
            matches.len() == 1,
            "SPOT_COIN={coin} 必须唯一匹配一个 HL spot 市场，实际匹配 {} 个",
            matches.len()
        );
        let rules = matches.remove(0);
        validate_market_identity(
            symbol,
            coin,
            &rules.asset,
            &rules.base_token,
            &rules.quote_token,
        )?;

        let exchange = ExchangeClient::new(None, wallet, Some(BaseUrl::Mainnet), None, None)
            .await
            .map_err(|e| anyhow!("Failed to create ExchangeClient: {e}"))?;
        anyhow::ensure!(
            exchange.coin_to_asset.contains_key(&rules.asset),
            "ExchangeClient 不支持已解析的 spot 资产 {}",
            rules.asset
        );

        Ok(Self {
            exchange,
            coin: rules.asset.clone(),
            rules,
        })
    }

    pub fn rules(&self) -> LiquidMarketRules {
        self.rules.clone()
    }

    pub async fn market_order(&self, is_buy: bool, price: f64, size: f64) -> Result<OrderResult> {
        let normalized = normalize_spot_order(is_buy, price, size, self.rules.size_decimals)?;
        let result = self
            .exchange
            .order(
                ClientOrderRequest {
                    asset: self.coin.clone(),
                    is_buy,
                    reduce_only: false,
                    limit_px: normalized.price,
                    sz: normalized.size,
                    cloid: None,
                    order_type: ClientOrder::Limit(ClientLimit { tif: "Ioc".into() }),
                },
                None,
            )
            .await
            .map_err(|e| anyhow!("market_order failed: {e}"))?;
        info!(
            is_buy,
            price = normalized.price,
            size = normalized.size,
            "[LiquidClient] market order placed"
        );
        Ok(Self::parse_response(result))
    }

    pub async fn cancel_order(&self, oid: u64) -> Result<()> {
        let result = self
            .exchange
            .cancel(
                ClientCancelRequest {
                    asset: self.coin.clone(),
                    oid,
                },
                None,
            )
            .await
            .map_err(|e| anyhow!("cancel failed: {e}"))?;
        info!(oid, "[LiquidClient] cancel order sent");
        ensure_cancel_succeeded(result)
    }

    fn parse_response(status: ExchangeResponseStatus) -> OrderResult {
        match status {
            ExchangeResponseStatus::Ok(resp) => {
                let Some(data) = resp.data else {
                    return OrderResult::with_status(OrderStatus::Failed);
                };
                for s in data.statuses {
                    match s {
                        ExchangeDataStatus::Filled(f) => {
                            let avg_price = f.avg_px.parse().ok();
                            let total_size = f.total_sz.parse().ok();
                            if avg_price.is_none() || total_size.is_none() {
                                // 订单已真实成交，仅数值解析失败；下游会因字段缺失走恢复流程，
                                // 必须留下原始值供人工核对，不能静默
                                error!(
                                    oid = f.oid,
                                    avg_px = %f.avg_px,
                                    total_sz = %f.total_sz,
                                    "[LiquidClient] Filled 响应数值解析失败——对冲已成交但金额字段无法解析"
                                );
                            }
                            return OrderResult {
                                status: OrderStatus::Filled,
                                oid: Some(f.oid),
                                avg_price,
                                total_size,
                            };
                        }
                        ExchangeDataStatus::Resting(r) => {
                            return OrderResult {
                                status: OrderStatus::Resting,
                                oid: Some(r.oid),
                                avg_price: None,
                                total_size: None,
                            };
                        }
                        ExchangeDataStatus::Error(e) => {
                            error!(error = %e, "[LiquidClient] order rejected by exchange");
                            return OrderResult::with_status(OrderStatus::Rejected);
                        }
                        s => {
                            warn!("[LiquidClient] 收到未识别的 ExchangeDataStatus: {s:?}，跳过");
                        }
                    }
                }
                warn!("[LiquidClient] 所有 status 均未识别，订单状态未知");
                OrderResult::with_status(OrderStatus::Unknown)
            }
            ExchangeResponseStatus::Err(e) => {
                warn!(error = %e, "[LiquidClient] exchange error response");
                OrderResult::with_status(OrderStatus::Rejected)
            }
        }
    }
}

/// HTTP 请求成功不代表撤单成功，必须检查响应内层 status。
pub fn ensure_cancel_succeeded(result: ExchangeResponseStatus) -> Result<()> {
    match result {
        ExchangeResponseStatus::Ok(response) => {
            let data = response
                .data
                .ok_or_else(|| anyhow!("cancel response missing data"))?;
            match data.statuses.as_slice() {
                [ExchangeDataStatus::Success] => Ok(()),
                [ExchangeDataStatus::Error(error)] => Err(anyhow!("cancel rejected: {error}")),
                statuses => Err(anyhow!("unexpected cancel statuses: {statuses:?}")),
            }
        }
        ExchangeResponseStatus::Err(error) => Err(anyhow!("cancel rejected: {error}")),
    }
}
