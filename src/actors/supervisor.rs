use anyhow::{bail, Result};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const GRACEFUL_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

async fn drain_tasks(tasks: &mut JoinSet<String>) -> Result<()> {
    let drain = async {
        while let Some(outcome) = tasks.join_next().await {
            if let Err(error) = outcome {
                return Err(anyhow::anyhow!("优雅关闭期间 actor task 异常: {error}"));
            }
        }
        Ok(())
    };

    match tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, drain).await {
        Ok(result) => result,
        Err(_) => {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            bail!(
                "优雅关闭超过 {} 秒，已强制终止剩余 actor；可能存在待恢复交易",
                GRACEFUL_SHUTDOWN_TIMEOUT.as_secs()
            )
        }
    }
}

/// 监督关键 actor。任一 actor 在全局取消前退出，都会停止整个进程。
pub async fn supervise(mut tasks: JoinSet<String>, shutdown: CancellationToken) -> Result<()> {
    tokio::select! {
            _ = shutdown.cancelled() => {
                drain_tasks(&mut tasks).await
            }
            outcome = tasks.join_next() => {
                match outcome {
                    Some(Ok(_name)) if shutdown.is_cancelled() => {
                        drain_tasks(&mut tasks).await
                    }
                    Some(Ok(name)) => {
                        shutdown.cancel();
                        let _ = drain_tasks(&mut tasks).await;
                        bail!("关键 actor 意外退出: {name}");
                    }
                    Some(Err(error)) => {
                        shutdown.cancel();
                        let _ = drain_tasks(&mut tasks).await;
                        Err(anyhow::anyhow!("关键 actor task 异常: {error}"))
                    }
                    None => {
                        shutdown.cancel();
                        bail!("没有可运行的关键 actor");
                    }
                }
            }
    }
}
