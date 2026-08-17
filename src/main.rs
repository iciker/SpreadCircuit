use std::future::Future;
use std::sync::Arc;
use tokio::{
    sync::{broadcast, mpsc, Mutex},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use spread_circuit::{
    actors::{
        arb_engine::ArbEngine, evm_trader, evm_watcher, liquid_trader, orderbook_watcher, price_db,
        supervisor::supervise,
    },
    config::Config,
    db,
    liquid::client::{validate_configured_size, LiquidClient},
    types::{
        commands::{EvmSwapRequest, LiquidCommand, TradeResult},
        market::{EvmPrice, OrderBook, PriceRecord},
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let config = Arc::new(Config::try_from_env()?);

    const LOG_FILE_PREFIX: &str = "app.log";
    std::fs::create_dir_all(&config.log_dir)
        .map_err(|e| anyhow::anyhow!("创建日志目录 '{}' 失败: {e}", config.log_dir))?;
    let file_appender = tracing_appender::rolling::daily(&config.log_dir, LOG_FILE_PREFIX);
    let (non_blocking_file, _file_guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::from_default_env();
    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::io::stdout),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(non_blocking_file),
        )
        .init();

    spread_circuit::metrics::init();

    let pairs = config.pairs.clone();
    let unresolved = db::list_recoveries_at(&config.db_path)?;
    db::ensure_startup_safe(config.dry_run, &unresolved)?;

    // 各 pair 的 HL 客户端构造相互独立，并发进行，启动时间不随 pair 数线性膨胀
    let prepared_pairs = futures_util::future::try_join_all(pairs.iter().map(|pair| async {
        let client = LiquidClient::new(&config.private_key, &pair.spot_coin, &pair.symbol).await?;
        let rules = client.rules();
        validate_configured_size(pair.order_size_token1, rules.size_decimals)?;
        anyhow::Ok((pair.clone(), client, rules))
    }))
    .await?;
    info!(
        dry_run = config.dry_run,
        metrics_port = config.metrics_port,
        pairs = pairs.len(),
        "[Main] 配置加载完成，启动所有 actor"
    );

    let shutdown = CancellationToken::new();
    let evm_execution_gate = Arc::new(Mutex::new(()));
    // 全进程共享一个报价 HTTP 连接池：watcher 每块报价保持连接温热，
    // trader 延迟敏感的 re-quote 免付冷启动 TCP/TLS 握手
    let quote_provider = spread_circuit::evm::client::build_http_provider(&config.https_rpc)?;
    let shutdown_ctrl_c = shutdown.clone();
    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            // 信号处理器注册失败：无法再感知 Ctrl-C，触发安全关闭而非静默运行
            error!(error = %e, "[Main] Ctrl-C 信号处理器注册失败，触发安全关闭");
        }
        info!("[Main] 收到退出信号，开始优雅关闭");
        shutdown_ctrl_c.cancel();
    });

    // === 全局广播 channel（所有 pair 共享，按 pair_id/symbol 过滤） ===
    // evm_price 每 ~2s 产生一条（每个 EVM 块），ArbEngine 几乎立即消费，8 条缓冲已足够
    let (evm_price_tx, _) = broadcast::channel::<EvmPrice>(8);
    let (orderbook_tx, _) = broadcast::channel::<OrderBook>(128);
    let (price_record_tx, _) = broadcast::channel::<PriceRecord>(128);

    let mut tasks = JoinSet::new();
    spawn_actor(
        &mut tasks,
        "metrics-server",
        spread_circuit::actors::metrics_server::run(
            config.metrics_host.clone(),
            config.metrics_port,
            shutdown.clone(),
        ),
    );
    spawn_actor(
        &mut tasks,
        "evm-watcher",
        evm_watcher::run(
            Arc::clone(&config),
            pairs.clone(),
            quote_provider.clone(),
            evm_price_tx.clone(),
            shutdown.clone(),
        ),
    );
    spawn_actor(
        &mut tasks,
        "price-db",
        price_db::run(
            Arc::clone(&config),
            price_record_tx.subscribe(),
            shutdown.clone(),
        ),
    );

    // OrderBookWatcher：单一 WS 连接订阅所有 pair 的订单簿
    let symbols: Vec<String> = pairs.iter().map(|p| p.symbol.clone()).collect();
    spawn_actor(
        &mut tasks,
        "orderbook-watcher",
        orderbook_watcher::run(
            Arc::clone(&config),
            symbols,
            orderbook_tx.clone(),
            shutdown.clone(),
        ),
    );

    // === 每个 pair 独立的 traders + ArbEngine ===
    for (pair, liquid_client, liquid_rules) in prepared_pairs {
        let pair = Arc::new(pair);

        // 每个 pair 独立的命令和结果 channel
        let (evm_cmd_tx, evm_cmd_rx) = mpsc::channel::<EvmSwapRequest>(16);
        let (liq_cmd_tx, liq_cmd_rx) = mpsc::channel::<LiquidCommand>(16);
        let (result_tx, result_rx) = mpsc::channel::<TradeResult>(32);

        // EVM 交易执行（使用 pair 的 token 地址）
        spawn_actor(
            &mut tasks,
            format!("evm-trader:{}", pair.symbol),
            evm_trader::run(
                Arc::clone(&config),
                Arc::clone(&pair),
                quote_provider.clone(),
                evm_cmd_rx,
                result_tx.clone(),
                evm_execution_gate.clone(),
                shutdown.clone(),
            ),
        );

        // HyperLiquid 下单执行（使用 pair 的 spot_coin）
        spawn_actor(
            &mut tasks,
            format!("liquid-trader:{}", pair.symbol),
            liquid_trader::run(
                liquid_client,
                pair.symbol.clone(),
                liq_cmd_rx,
                result_tx.clone(),
            ),
        );

        // 套利引擎（订阅全局 broadcast，按 pair_id/symbol 过滤）
        let arb_name = format!("arb-engine:{}", pair.symbol);
        let arb = ArbEngine::new(
            Arc::clone(&config),
            pair,
            evm_price_tx.subscribe(),
            orderbook_tx.subscribe(),
            evm_cmd_tx,
            liq_cmd_tx,
            result_rx,
            price_record_tx.clone(),
            liquid_rules,
        )?;
        spawn_actor(&mut tasks, arb_name, arb.run(shutdown.clone()));
    }

    supervise(tasks, shutdown).await?;
    info!("[Main] 所有 actor 已退出，程序结束");
    Ok(())
}

fn spawn_actor(
    tasks: &mut JoinSet<String>,
    name: impl Into<String>,
    actor: impl Future<Output = ()> + Send + 'static,
) {
    let name = name.into();
    tasks.spawn(async move {
        actor.await;
        name
    });
}
