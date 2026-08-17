use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    config::{Config, PairConfig},
    evm::{
        algebra_client::AlgebraQuoterClient,
        client::{build_http_provider, EvmClient},
    },
    types::market::{unix_timestamp_ms, DexName, EvmPrice},
};

/// 报价基准金额（100 个 token0 单位），buy_price = QUOTE_AMOUNT / token_out
const QUOTE_AMOUNT: f64 = 100.0;

pub async fn run(
    config: Arc<Config>,
    pairs: Vec<PairConfig>,
    tx: broadcast::Sender<EvmPrice>,
    shutdown: CancellationToken,
) {
    super::run_with_backoff(
        || connect_and_listen(&config, &pairs, &tx, &shutdown),
        "EvmWatcher",
        &shutdown,
    )
    .await;
    info!("[EvmWatcher] 已退出");
}

async fn connect_and_listen(
    config: &Config,
    pairs: &[PairConfig],
    tx: &broadcast::Sender<EvmPrice>,
    shutdown: &CancellationToken,
) -> Result<()> {
    // 三个 client 共享同一 HTTP provider，避免为相同 RPC URL 创建多个连接池
    let http_provider = build_http_provider(&config.https_rpc)?;
    let prjx_client = EvmClient::from_provider(http_provider.clone(), &config.prjx_quotev2)?;
    let hyper_client = EvmClient::from_provider(http_provider.clone(), &config.hyperswap_quotev2)?;
    let kitten_client = if config.kitten_quoter.is_empty() {
        None
    } else if config.kitten_router.is_empty() {
        // KITTEN_QUOTER 有值但 KITTEN_ROUTER 为空：报价会选出 DexName::Kitten，
        // 但每次套利触发都会因 router 缺失而失败，产生大量虚假错误指标。
        // 强制禁用 kitten 报价，直到两者均配置。
        tracing::error!("[EvmWatcher] KITTEN_QUOTER 已配置但 KITTEN_ROUTER 为空，KittenSwap 已禁用。请同时配置 KITTEN_QUOTER 和 KITTEN_ROUTER，或两者均不配置");
        None
    } else {
        Some(AlgebraQuoterClient::from_provider(
            http_provider,
            &config.kitten_quoter,
        )?)
    };

    let ws = WsConnect::new(&config.wss_rpc);
    let provider = ProviderBuilder::new().on_ws(ws).await?;
    let mut subscription = provider.subscribe_blocks().await?;
    info!(pairs = pairs.len(), "[EvmWatcher] WebSocket 已连接");
    for pair in pairs {
        let mut dexes = vec![DexName::Prjx.as_str()];
        if pair.use_hyperswap {
            dexes.push(DexName::HyperSwap.as_str());
        }
        if pair.use_kitten && kitten_client.is_some() {
            dexes.push(DexName::Kitten.as_str());
        }
        info!(symbol = %pair.symbol, dexes = ?dexes, "[EvmWatcher] pair DEX 配置");
    }

    loop {
        tokio::select! {
            block = subscription.recv() => {
                let block = block?;
                let block_number = block.number;

                // 所有 pair 并行报价：调用侧将 bool 转为 Option，fetch_prices 不感知 pair 配置
                let futures: Vec<_> = pairs.iter().map(|pair| {
                    let hyper = if pair.use_hyperswap { Some(&hyper_client) } else { None };
                    let kitten = if pair.use_kitten { kitten_client.as_ref() } else { None };
                    fetch_prices(&prjx_client, hyper, kitten, pair, block_number)
                }).collect();

                let results = match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    futures_util::future::join_all(futures),
                ).await {
                    Ok(results) => results,
                    Err(_) => {
                        warn!(block_number, "[EvmWatcher] 本区块报价超时，整批丢弃");
                        continue;
                    }
                };
                crate::metrics::record_evm_block(block_number);
                for (pair, result) in pairs.iter().zip(results) {
                    match result {
                        Ok(price) => {
                            info!(
                                block_number,
                                buy_price = price.buy_price,
                                sell_price = price.sell_price,
                                sell_dex = %price.sell_dex,
                                buy_dex = %price.buy_dex,
                                pair_id = %pair.symbol,
                                "[EvmWatcher] 新块价格"
                            );
                            crate::metrics::record_evm_price(&pair.symbol, price.buy_price, price.sell_price);
                            if tx.send(price).is_err() {
                                warn!(pair_id = %pair.symbol, "[EvmWatcher] 无订阅者，价格已丢弃");
                            }
                        }
                        Err(e) => warn!(error = %e, pair_id = %pair.symbol, "[EvmWatcher] 报价失败"),
                    }
                }
            }
            _ = shutdown.cancelled() => return Ok(()),
        }
    }
}

async fn fetch_prices(
    prjx: &EvmClient,
    hyper: Option<&EvmClient>,
    kitten: Option<&AlgebraQuoterClient>,
    pair: &PairConfig,
    block_number: u64,
) -> Result<EvmPrice> {
    // 外层并行所有 DEX，内层并行每个 DEX 的买卖报价。
    let (prjx_quotes, hyper_quotes, kitten_quotes) = tokio::join!(
        quote_uniswap_pair(prjx, pair),
        quote_optional_uniswap_pair(hyper, pair),
        quote_optional_algebra_pair(kitten, pair),
    );

    let mut candidates_buy = Vec::with_capacity(3);
    let mut candidates_sell = Vec::with_capacity(3);
    push_quote_pair(
        &mut candidates_buy,
        &mut candidates_sell,
        DexName::Prjx,
        Some(prjx_quotes),
    );
    push_quote_pair(
        &mut candidates_buy,
        &mut candidates_sell,
        DexName::HyperSwap,
        hyper_quotes,
    );
    push_quote_pair(
        &mut candidates_buy,
        &mut candidates_sell,
        DexName::Kitten,
        kitten_quotes,
    );

    let (buy_raw, buy_dex) = best_of(candidates_buy, true, "buy")?;
    let (sell_raw, sell_dex) = best_of(candidates_sell, false, "sell")?;

    Ok(EvmPrice {
        buy_price: QUOTE_AMOUNT / buy_raw,
        sell_price: QUOTE_AMOUNT / sell_raw,
        block_number,
        received_at: unix_timestamp_ms(),
        sell_dex,
        buy_dex,
        pair_id: pair.symbol.clone(),
    })
}

type QuotePair = (Result<f64>, Result<f64>);

async fn quote_uniswap_pair(client: &EvmClient, pair: &PairConfig) -> QuotePair {
    tokio::join!(
        client.quote_exact_input(
            &pair.token0,
            &pair.token1,
            pair.decimals0,
            pair.decimals1,
            pair.fee_tier,
            QUOTE_AMOUNT,
        ),
        client.quote_exact_output(
            &pair.token1,
            &pair.token0,
            pair.decimals1,
            pair.decimals0,
            pair.fee_tier,
            QUOTE_AMOUNT,
        ),
    )
}

async fn quote_optional_uniswap_pair(
    client: Option<&EvmClient>,
    pair: &PairConfig,
) -> Option<QuotePair> {
    match client {
        Some(client) => Some(quote_uniswap_pair(client, pair).await),
        None => None,
    }
}

async fn quote_optional_algebra_pair(
    client: Option<&AlgebraQuoterClient>,
    pair: &PairConfig,
) -> Option<QuotePair> {
    let client = client?;
    Some(tokio::join!(
        client.quote_exact_input(
            &pair.token0,
            &pair.token1,
            pair.decimals0,
            pair.decimals1,
            QUOTE_AMOUNT,
        ),
        client.quote_exact_output(
            &pair.token1,
            &pair.token0,
            pair.decimals1,
            pair.decimals0,
            QUOTE_AMOUNT,
        ),
    ))
}

fn push_quote_pair(
    buy_candidates: &mut Vec<(Result<f64>, DexName)>,
    sell_candidates: &mut Vec<(Result<f64>, DexName)>,
    dex: DexName,
    quotes: Option<QuotePair>,
) {
    if let Some((buy, sell)) = quotes {
        buy_candidates.push((buy, dex));
        sell_candidates.push((sell, dex));
    }
}

/// buy=true 取最大值（更多 tokenOut），buy=false 取最小值（更少 tokenIn）
/// 至少一个成功才返回 Ok
fn best_of(
    candidates: Vec<(Result<f64>, DexName)>,
    prefer_max: bool,
    label: &str,
) -> Result<(f64, DexName)> {
    let mut best: Option<(f64, DexName)> = None;
    let mut last_err: Option<String> = None;

    for (result, dex) in candidates {
        match result {
            Ok(v) if v > 0.0 => {
                let is_better = match best {
                    None => true,
                    Some((cur, _)) => {
                        if prefer_max {
                            v > cur
                        } else {
                            v < cur
                        }
                    }
                };
                if is_better {
                    best = Some((v, dex));
                }
            }
            Ok(v) => {
                // 报价为 0 意味着池子无流动性，视为无效结果
                warn!(
                    v,
                    dex = dex.as_str(),
                    label,
                    "[EvmWatcher] DEX 报价为零，跳过"
                );
                last_err = Some(format!("{dex}: 报价为零"));
            }
            Err(e) => {
                warn!(error = %e, dex = dex.as_str(), label, "[EvmWatcher] DEX 报价失败");
                last_err = Some(format!("{dex}: {e}"));
            }
        }
    }

    best.ok_or_else(|| {
        anyhow::anyhow!(
            "所有 DEX {label} 报价均失败: {}",
            last_err.unwrap_or_default()
        )
    })
}
