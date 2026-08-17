use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{config::Config, db, types::market::PriceRecord};

const PRUNE_EVERY_INSERTS: usize = 1_000;
const MAX_CONSECUTIVE_WRITE_FAILURES: usize = 3;

pub async fn run(
    config: Arc<Config>,
    mut rx: broadcast::Receiver<PriceRecord>,
    shutdown: CancellationToken,
) {
    let db_path = config.db_path.clone();
    let conn = match tokio::task::spawn_blocking(move || {
        let conn = rusqlite::Connection::open(&db_path)?;
        db::init(&conn)?;
        anyhow::Ok(conn)
    })
    .await
    {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            error!(error = %e, "[PriceDb] 初始化失败，退出");
            return;
        }
        Err(e) => {
            error!(error = %e, "[PriceDb] spawn_blocking 失败，退出");
            return;
        }
    };
    if let Err(error) = db::prune_prices(&conn, config.db_max_price_rows) {
        error!(error = %error, "[PriceDb] 启动时裁剪失败，退出");
        return;
    }
    let mut successful_inserts = 0_usize;
    let mut consecutive_failures = 0_usize;

    loop {
        tokio::select! {
            result = rx.recv() => match result {
                Ok(record) => {
                    if let Err(e) = tokio::task::block_in_place(|| db::insert_price(&conn, &record)) {
                        error!(error = %e, "[PriceDb] insert 失败");
                        consecutive_failures += 1;
                        if consecutive_failures >= MAX_CONSECUTIVE_WRITE_FAILURES {
                            error!("[PriceDb] 连续写入失败达到上限，退出并触发全局安全关闭");
                            break;
                        }
                    } else {
                        consecutive_failures = 0;
                        successful_inserts += 1;
                        if successful_inserts % PRUNE_EVERY_INSERTS == 0 {
                            match tokio::task::block_in_place(|| db::prune_prices(&conn, config.db_max_price_rows)) {
                                Ok(deleted) if deleted > 0 => {
                                    info!(deleted, max_rows = config.db_max_price_rows, "[PriceDb] 已裁剪旧价格记录");
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    error!(error = %error, "[PriceDb] 定期裁剪失败，退出并触发全局安全关闭");
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(n, "[PriceDb] 消息滞后，跳过 {n} 条记录");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("[PriceDb] 发送端已关闭，退出");
                    break;
                }
            },
            _ = shutdown.cancelled() => break,
        }
    }
    info!("[PriceDb] 已退出");
}
