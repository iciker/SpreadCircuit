use alloy::{
    network::EthereumWallet,
    primitives::Address,
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    transports::Transport,
};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{
    config::{Config, PairConfig},
    evm::{
        algebra_client::{algebra_swap_exact_input_single, AlgebraQuoterClient},
        client::{
            build_http_provider, erc20_balance, from_bigint, swap_exact_input_single, to_bigint,
            EvmClient, SwapOutcome, SwapRequest,
        },
    },
    types::{
        commands::{EvmSwapRequest, TradeResult},
        market::DexName,
    },
};

use super::BPS;

/// 价格恶化错误前缀，用于区分"盈利验证放弃"（不发 tx）和"链上失败"两类错误。
///
/// **约定**：`execute_swap` 的 `bail!` 消息必须以此常量开头，
/// 调用方通过 `starts_with(PRICE_DETERIORATED)` 匹配。
/// 修改此值或 `bail!` 格式时须同步更新两处，否则匹配静默失效。
pub(crate) const PRICE_DETERIORATED: &str = "价格已恶化";
const EXECUTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// 链上 `amountOutMinimum` 同时受滑点和经济盈亏平衡线约束，不能低于任一者。
pub fn guarded_amount_out_min(
    fresh_out: f64,
    economic_floor: f64,
    min_out_bps: f64,
) -> Result<f64> {
    anyhow::ensure!(
        fresh_out.is_finite() && fresh_out > 0.0,
        "fresh_out 必须为正有限数"
    );
    anyhow::ensure!(
        economic_floor.is_finite() && economic_floor > 0.0,
        "economic_floor 必须为正有限数"
    );
    anyhow::ensure!(
        min_out_bps.is_finite() && (0.0..BPS).contains(&min_out_bps),
        "min_out_bps 必须位于 [0, 10000)"
    );
    anyhow::ensure!(fresh_out >= economic_floor, "{PRICE_DETERIORATED}");
    Ok((fresh_out * (1.0 - min_out_bps / BPS)).max(economic_floor))
}

pub async fn run(
    config: Arc<Config>,
    pair: Arc<PairConfig>,
    mut rx: mpsc::Receiver<EvmSwapRequest>,
    tx: mpsc::Sender<TradeResult>,
    execution_gate: Arc<Mutex<()>>,
    _shutdown: CancellationToken,
) {
    let signer: PrivateKeySigner = match config.private_key.parse() {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "[EvmTrader] 私钥解析失败，退出");
            return;
        }
    };
    let address = signer.address();
    let wallet = EthereumWallet::from(signer);
    let url = match config.https_rpc.parse() {
        Ok(u) => u,
        Err(e) => {
            error!(error = %e, "[EvmTrader] RPC URL 解析失败，退出");
            return;
        }
    };

    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_http(url);

    // 报价客户端：独立 HTTP provider，无需签名，仅用于执行前实时 re-quote
    let (prjx_quoter, hyper_quoter, kitten_quoter) = match build_quoters(&config) {
        Ok(q) => q,
        Err(e) => {
            error!(error = %e, "[EvmTrader] 报价客户端初始化失败，退出");
            return;
        }
    };
    while let Some(cmd) = rx.recv().await {
        let _wallet_guard = execution_gate.lock().await;
        let execution = tokio::time::timeout(
            EXECUTION_TIMEOUT,
            execute_swap(
                &provider,
                &prjx_quoter,
                &hyper_quoter,
                kitten_quoter.as_ref(),
                &config,
                &pair,
                address,
                &cmd,
            ),
        )
        .await;
        let result = match execution {
            Ok(result) => map_swap_result(&pair, &cmd, result),
            Err(_) => TradeResult::EvmSwapUnknown {
                reason: "EVM 交易执行超时".into(),
            },
        };
        if tx.send(result).await.is_err() {
            error!("[EvmTrader] result channel closed");
            break;
        }
    }
    info!(symbol = %pair.symbol, "[EvmTrader] command channel 已关闭，退出");
}

fn map_swap_result(
    pair: &PairConfig,
    request: &EvmSwapRequest,
    result: Result<(SwapOutcome, DexName)>,
) -> TradeResult {
    match result {
        Ok((outcome, execution_dex)) => {
            info!(amount_in = request.amount_in, amount_out = outcome.amount_out, tx_hash = %outcome.tx_hash, dex = %execution_dex, quoted_dex = %request.dex, symbol = %pair.symbol, "[EvmTrader] swap 成功");
            crate::metrics::record_evm_swap_success(&pair.symbol, outcome.amount_out);
            TradeResult::EvmSwapSuccess {
                amount_out: outcome.amount_out,
                tx_hash: outcome.tx_hash,
            }
        }
        Err(error) => {
            let reason = error.to_string();
            if reason.starts_with(PRICE_DETERIORATED) {
                warn!(reason, dex = %request.dex, symbol = %pair.symbol, "[EvmTrader] re-quote 后放弃 swap");
                crate::metrics::record_requote_aborted(&pair.symbol);
            } else {
                error!(reason, dex = %request.dex, symbol = %pair.symbol, "[EvmTrader] swap 失败");
                crate::metrics::record_evm_swap_failed(&pair.symbol);
            }
            TradeResult::EvmSwapFailed { reason }
        }
    }
}

// 显式传入只读报价资源，避免恢复仅用于这一处调用的依赖包装结构。
#[allow(clippy::too_many_arguments)]
async fn execute_swap<T, P>(
    provider: &P,
    prjx_quoter: &EvmClient,
    hyper_quoter: &EvmClient,
    kitten_quoter: Option<&AlgebraQuoterClient>,
    config: &Config,
    pair: &PairConfig,
    address: Address,
    request: &EvmSwapRequest,
) -> Result<(crate::evm::client::SwapOutcome, DexName)>
where
    T: Transport + Clone,
    P: Provider<T>,
{
    let (token_in_str, token_out_str, decimals_in, decimals_out) = if request.is_buy {
        (
            pair.token0.as_str(),
            pair.token1.as_str(),
            pair.decimals0,
            pair.decimals1,
        )
    } else {
        (
            pair.token1.as_str(),
            pair.token0.as_str(),
            pair.decimals1,
            pair.decimals0,
        )
    };
    let token_in: Address = token_in_str.parse()?;
    let token_out: Address = token_out_str.parse()?;

    // ── 执行前实时 re-quote（优先于 balance/nonce 查询）────────────────────
    // 两个历史问题：
    //   1. 基准金额不一致：EvmWatcher 用 QUOTE_AMOUNT=100 报价，实际执行金额不同，
    //      两者价格冲击不同，导致 amountOutMinimum 计算有误差
    //   2. 价格陈旧：EvmWatcher 在上一个块报价，到 swap 上链至少经过 1-3 秒，
    //      价格可能已移动超过 min_out_bps，导致链上 STF 回滚
    // 放在 balance/nonce 前：价格恶化是高频路径，提前退出可省两次 RPC 往返
    let (fresh_out, execution_dex) = requote_best(
        prjx_quoter,
        hyper_quoter,
        kitten_quoter,
        pair,
        request.is_buy,
        request.amount_in,
    )
    .await?;

    // 盈利验证：fresh_out < target_amount 意味着当前价格已低于套利盈亏平衡点，
    // 本次套利机会已消失，直接放弃（不发 tx，节省 gas）
    if fresh_out < request.target_amount {
        let deterioration_bps = (request.target_amount - fresh_out) / request.target_amount * BPS;
        anyhow::bail!(
            "{PRICE_DETERIORATED}，放弃 swap: fresh={fresh_out:.6} < target={:.6} (恶化 {deterioration_bps:.2}bps)",
            request.target_amount,
        );
    }

    // 用实时报价重新计算 amountOutMinimum：防止 tx 提交到上链期间的价格微小滑动
    let amount_out_min =
        guarded_amount_out_min(fresh_out, request.target_amount, pair.min_out_bps)?;

    info!(
        fresh_out, target_amount = request.target_amount, amount_out_min,
        dex = %execution_dex, quoted_dex = %request.dex, symbol = %pair.symbol,
        "[EvmTrader] re-quote 通过，发送 swap tx"
    );

    let amount_in_raw = to_bigint(request.amount_in, decimals_in)?;

    // 余额检查与 pending nonce 查询并行，两者相互独立
    // 每次 swap 显式注入链上 pending nonce，避免 alloy CachedNonceProvider 缓存
    // 因边界情况（如长时间未发 tx）与链不同步，导致 -32003 nonce too low
    let (balance, nonce) = tokio::join!(
        erc20_balance(provider, token_in, address),
        provider.get_transaction_count(address).pending(),
    );
    let balance = balance?;
    let nonce = nonce?;
    if balance < amount_in_raw {
        let balance_f = from_bigint(balance, decimals_in).unwrap_or(0.0);
        anyhow::bail!(
            "{} 余额不足: 有 {balance_f:.4}，需要 {:.4}，跳过",
            pair.symbol,
            request.amount_in,
        );
    }

    let (router_str, router_label, fee_tier) = match execution_dex {
        DexName::Kitten => (config.kitten_router.as_str(), "kitten_router", 0),
        DexName::Prjx => (
            config.prjx_routerv2.as_str(),
            "prjx_routerv2",
            pair.fee_tier,
        ),
        DexName::HyperSwap => (
            config.hyperswap_router01.as_str(),
            "hyperswap_router01",
            pair.fee_tier,
        ),
    };
    anyhow::ensure!(
        !router_str.is_empty(),
        "{router_label} 未配置，无法执行 swap"
    );
    let router: Address = router_str.parse()?;
    anyhow::ensure!(
        router != Address::ZERO,
        "{router_label} 解析为零地址，配置无效"
    );
    let swap_request = SwapRequest {
        router,
        token_in,
        token_out,
        decimals_in,
        decimals_out,
        fee_tier,
        recipient: address,
        amount_in: request.amount_in,
        amount_out_min,
    };

    let amount_out = match execution_dex {
        DexName::Kitten => algebra_swap_exact_input_single(provider, &swap_request, nonce).await,
        // PRJX 和 HyperSwap 都使用 Uniswap V3 兼容接口（ISwapRouter01）。
        DexName::Prjx | DexName::HyperSwap => {
            swap_exact_input_single(provider, &swap_request, nonce).await
        }
    }?;
    Ok((amount_out, execution_dex))
}

/// 初始化三个 DEX 报价客户端，共享同一 HTTP 连接池
fn build_quoters(config: &Config) -> Result<(EvmClient, EvmClient, Option<AlgebraQuoterClient>)> {
    let http = build_http_provider(&config.https_rpc)?;
    let prjx_q = EvmClient::from_provider(http.clone(), &config.prjx_quotev2)?;
    let hyper_q = EvmClient::from_provider(http.clone(), &config.hyperswap_quotev2)?;
    let kitten_q = if !config.kitten_quoter.is_empty() && !config.kitten_router.is_empty() {
        Some(AlgebraQuoterClient::from_provider(
            http,
            &config.kitten_quoter,
        )?)
    } else {
        None
    };
    Ok((prjx_q, hyper_q, kitten_q))
}

/// 对实际交易金额做实时链上报价
/// 与 EvmWatcher 的 fetch_prices 区别：
///   - 使用真实 amount_in（非基准 QUOTE_AMOUNT=100）
///   - 在 swap 执行前调用，反映当前块状态
async fn requote_best(
    prjx_quoter: &EvmClient,
    hyper_quoter: &EvmClient,
    kitten_quoter: Option<&AlgebraQuoterClient>,
    pair: &PairConfig,
    is_buy: bool,
    amount_in: f64,
) -> Result<(f64, DexName)> {
    let (token_in, token_out, decimals_in, decimals_out) = if is_buy {
        (
            pair.token0.as_str(),
            pair.token1.as_str(),
            pair.decimals0,
            pair.decimals1,
        )
    } else {
        (
            pair.token1.as_str(),
            pair.token0.as_str(),
            pair.decimals1,
            pair.decimals0,
        )
    };
    let (prjx, hyper, kitten) = tokio::join!(
        prjx_quoter.quote_exact_input(
            token_in,
            token_out,
            decimals_in,
            decimals_out,
            pair.fee_tier,
            amount_in,
        ),
        async {
            if pair.use_hyperswap {
                hyper_quoter
                    .quote_exact_input(
                        token_in,
                        token_out,
                        decimals_in,
                        decimals_out,
                        pair.fee_tier,
                        amount_in,
                    )
                    .await
                    .ok()
            } else {
                None
            }
        },
        async {
            if pair.use_kitten {
                match kitten_quoter {
                    Some(quoter) => quoter
                        .quote_exact_input(
                            token_in,
                            token_out,
                            decimals_in,
                            decimals_out,
                            amount_in,
                        )
                        .await
                        .ok(),
                    None => None,
                }
            } else {
                None
            }
        }
    );

    let candidates = [
        (DexName::Prjx, prjx.ok()),
        (DexName::HyperSwap, hyper),
        (DexName::Kitten, kitten),
    ];
    select_best_executable_quote(&candidates)
}

/// 从实际下单量的最新报价中选择输出最多的可执行 DEX。
pub fn select_best_executable_quote(
    candidates: &[(DexName, Option<f64>)],
) -> Result<(f64, DexName)> {
    candidates
        .iter()
        .filter_map(|(dex, amount)| {
            amount
                .filter(|value| value.is_finite() && *value > 0.0)
                .map(|value| (value, *dex))
        })
        .max_by(|(left, _), (right, _)| left.total_cmp(right))
        .ok_or_else(|| anyhow::anyhow!("所有启用 DEX 的实时报价均失败或无效"))
}
