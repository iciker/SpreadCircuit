use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    config::{Config, PairConfig},
    evm::{algebra_client::AlgebraQuoterClient, build_quoters, client::EvmClient},
    types::market::{unix_timestamp_ms, DexName, EvmPrice},
};

/// 报价基准金额（100 个 token0 单位），buy_price = QUOTE_AMOUNT / token_out
const QUOTE_AMOUNT: f64 = 100.0;
/// 无新块超过此时长视为半开连接，断开重连（HyperEVM 出块间隔约 2 秒）
const BLOCK_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub async fn run(
    config: Arc<Config>,
    pairs: Vec<PairConfig>,
    quote_provider: crate::evm::client::EthProvider,
    tx: broadcast::Sender<EvmPrice>,
    shutdown: CancellationToken,
) {
    super::run_with_backoff(
        || connect_and_listen(&config, &pairs, quote_provider.clone(), &tx, &shutdown),
        "EvmWatcher",
        &shutdown,
        &config,
    )
    .await;
    info!("[EvmWatcher] 已退出");
}

async fn connect_and_listen(
    config: &Config,
    pairs: &[PairConfig],
    quote_provider: crate::evm::client::EthProvider,
    tx: &broadcast::Sender<EvmPrice>,
    shutdown: &CancellationToken,
) -> Result<()> {
    // 三个 client 共享进程级 HTTP provider；kitten 仅在配置了 KITTEN_QUOTER 时启用
    let (prjx_client, hyper_client, kitten_client) = build_quoters(quote_provider, config)?;

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
            block = tokio::time::timeout(BLOCK_IDLE_TIMEOUT, subscription.recv()) => {
                let block = match block {
                    Ok(block) => block?,
                    Err(_) => anyhow::bail!(
                        "超过 {BLOCK_IDLE_TIMEOUT:?} 未收到新块，判定半开连接，重连"
                    ),
                };
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

    let (candidates_buy, candidates_sell): (Vec<_>, Vec<_>) = [
        (DexName::Prjx, Some(prjx_quotes)),
        (DexName::HyperSwap, hyper_quotes),
        (DexName::Kitten, kitten_quotes),
    ]
    .into_iter()
    .filter_map(|(dex, quotes)| quotes.map(|(buy, sell)| ((buy, dex), (sell, dex))))
    .unzip();

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

/// buy=true 取最大值（更多 tokenOut），buy=false 取最小值（更少 tokenIn）
/// 至少一个成功才返回 Ok；失败/为零（无流动性）的报价逐条 warn 后跳过
fn best_of(
    candidates: Vec<(Result<f64>, DexName)>,
    prefer_max: bool,
    label: &str,
) -> Result<(f64, DexName)> {
    let valid = candidates
        .into_iter()
        .filter_map(|(result, dex)| match result {
            Ok(v) if crate::evm::is_valid_quote(v) => Some((v, dex)),
            Ok(v) => {
                // 报价为 0 意味着池子无流动性，NaN/inf 为异常返回，均视为无效
                warn!(
                    v,
                    dex = dex.as_str(),
                    label,
                    "[EvmWatcher] DEX 报价无效，跳过"
                );
                None
            }
            Err(e) => {
                warn!(error = %e, dex = dex.as_str(), label, "[EvmWatcher] DEX 报价失败");
                None
            }
        });
    let best = if prefer_max {
        valid.max_by(|(a, _), (b, _)| a.total_cmp(b))
    } else {
        valid.min_by(|(a, _), (b, _)| a.total_cmp(b))
    };
    best.ok_or_else(|| anyhow::anyhow!("所有 DEX {label} 报价均失败"))
}
