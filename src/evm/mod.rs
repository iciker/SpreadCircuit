pub mod algebra_client;
pub mod client;

use algebra_client::AlgebraQuoterClient;
use anyhow::Result;
use client::{EthProvider, EvmClient};

use crate::config::Config;

/// 报价有效性判据：所有选路逻辑共用，避免各处判据漂移
pub fn is_valid_quote(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

/// 初始化三个 DEX 报价客户端，复用调用方传入的 HTTP 连接池
/// （watcher 每块报价保持连接温热，trader re-quote 直接受益）。
/// kitten 仅在配置了 KITTEN_QUOTER 时构建（Config 校验保证 quoter/router 同缺同存）。
pub fn build_quoters(
    provider: EthProvider,
    config: &Config,
) -> Result<(EvmClient, EvmClient, Option<AlgebraQuoterClient>)> {
    let prjx = EvmClient::from_provider(provider.clone(), &config.prjx_quotev2)?;
    let hyper = EvmClient::from_provider(provider.clone(), &config.hyperswap_quotev2)?;
    let kitten = if config.kitten_quoter.is_empty() {
        None
    } else {
        Some(AlgebraQuoterClient::from_provider(
            provider,
            &config.kitten_quoter,
        )?)
    };
    Ok((prjx, hyper, kitten))
}
