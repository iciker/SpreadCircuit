/// 万分之一（basis points）换算基数，被 arb_engine / evm_trader 共用
pub(super) const BPS: f64 = 10000.0;

pub mod arb_engine;
pub mod evm_trader;
pub mod evm_watcher;
pub mod liquid_trader;
pub mod metrics_server;
pub mod orderbook_watcher;
pub mod price_db;
pub mod supervisor;

use tokio_util::sync::CancellationToken;

/// 指数退避重连循环：connect_fn 返回 Ok 时退出，返回 Err 时等待重连
/// 两端都通过 shutdown token 感知退出信号。
/// 错误日志统一经 config 脱敏——transport 错误可能内嵌带凭据的 RPC URL，
/// 在共享日志出口收口，接入方无需各自记得包脱敏。
pub async fn run_with_backoff<F, Fut>(
    mut connect_fn: F,
    module: &str,
    shutdown: &CancellationToken,
    config: &crate::config::Config,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    /// 连接存活超过此时长说明上次退避已奏效，重置退避，避免延迟单调恶化到 60s
    const BACKOFF_RESET_AFTER: std::time::Duration = std::time::Duration::from_secs(60);
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        let connected_at = std::time::Instant::now();
        match connect_fn().await {
            Ok(_) => break,
            Err(e) => {
                if connected_at.elapsed() >= BACKOFF_RESET_AFTER {
                    backoff = std::time::Duration::from_secs(1);
                }
                let error = config.redact_rpc(&format!("{e:#}"));
                tracing::error!(error, "[{module}] 连接错误");
                if shutdown.is_cancelled() {
                    break;
                }
                tracing::warn!(delay_secs = backoff.as_secs(), "[{module}] 重连中...");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown.cancelled() => break,
                }
                backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
            }
        }
    }
}
