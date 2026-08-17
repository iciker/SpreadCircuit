use axum::{http::header, response::IntoResponse, routing::get, Router};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

async fn handler() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        crate::metrics::gather(),
    )
}

pub async fn run(host: String, port: u16, shutdown: CancellationToken) {
    let app = Router::new().route("/metrics", get(handler));
    let addr = format!("{host}:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(error = %e, "[MetricsServer] 绑定地址失败");
            return;
        }
    };
    if host != "127.0.0.1" {
        tracing::warn!(
            addr,
            "[MetricsServer] 监听非本地地址，/metrics 数据（价差/仓位）对外网可见"
        );
    }
    info!(addr, "[MetricsServer] 启动，监听 /metrics");
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
    {
        error!(error = %e, "[MetricsServer] 服务异常退出");
    }
    info!("[MetricsServer] 已退出");
}
