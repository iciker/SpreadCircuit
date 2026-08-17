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
/// 两端都通过 shutdown token 感知退出信号
pub async fn run_with_backoff<F, Fut>(mut connect_fn: F, module: &str, shutdown: &CancellationToken)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        match connect_fn().await {
            Ok(_) => break,
            Err(e) => {
                tracing::error!(error = %e, "[{module}] 连接错误");
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
