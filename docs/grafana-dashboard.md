# Grafana 监控面板说明

本文档说明 SpreadCircuit 套利机器人的 Prometheus 指标含义及推荐的 Grafana 面板配置。

可直接导入的面板文件：[grafana-dashboard.json](grafana-dashboard.json)

导入方式：在 Grafana 打开 **Dashboards → New → Import**，上传 JSON 文件，在提示时选择 Prometheus 数据源，然后点击 **Import**。面板提供 `job`、`instance` 和 `pair` 三个筛选变量。

---

## 一、数据接入

**Prometheus 抓取配置**（prometheus.yml）：

```yaml
scrape_configs:
  - job_name: 'spread-circuit'
    static_configs:
      - targets: ['localhost:9090']   # 对应 .env METRICS_PORT
    scrape_interval: 5s
```

Prometheus → Grafana：在 Grafana 中添加 Prometheus 数据源，地址填 Prometheus 服务地址。

---

## 二、指标速查表

| 指标名 | 类型 | Labels | 含义 |
|--------|------|--------|------|
| `evm_price_usdc` | GaugeVec | `side=buy\|sell`, `pair` | EVM DEX 聚合最优价（USDC per token1） |
| `hl_price_usdc` | GaugeVec | `side=ask\|bid`, `pair` | HL 订单簿 ask/bid 价格（USDC per token1） |
| `evm_latest_block_number` | Gauge | — | 最新处理的 EVM 区块号（判断数据是否滞后） |
| `arb_price_diff_bps` | GaugeVec | `direction=buy\|sell`, `pair` | 当前价差（bps），每个新块/新订单更新 |
| `arb_min_profit_bps` | GaugeVec | `pair` | 当前 `buy_diff` 动态经济下限（bps）；源码暂未单独暴露 sell 下限 |
| `arb_trigger_total` | CounterVec | `direction=buy_diff\|sell_diff`, `pair` | 真实套利触发次数 |
| `arb_dry_run_trigger_total` | CounterVec | `direction=buy_diff\|sell_diff`, `pair` | DRY_RUN 模式下满足条件的次数 |
| `evm_swap_total` | CounterVec | `result=success\|failed\|aborted\|unknown`, `pair` | EVM Swap 结果统计（aborted=发送前安全放弃，unknown=超时/金额未知） |
| `evm_requote_aborted_total` | CounterVec | `pair` | 执行前重新报价后因价格恶化而放弃、未发送链上交易的次数 |
| `evm_swap_amount_out_total` | CounterVec | `direction=buy_diff\|sell_diff`, `pair` | 累计产出量（buy_diff → USDC，sell_diff → token1；单位不同勿跨 direction 聚合） |
| `arb_recovery_required_total` | CounterVec | `pair` | 进入 RecoveryRequired 的次数——**应配置最高优先级告警（>0 即告警）** |
| `liquid_order_total` | CounterVec | `result=filled\|resting\|failed`, `pair` | HL 限价单结果统计 |
| `liquid_filled_size_total` | CounterVec | `pair` | HL 累计成交数量（token1） |
| `arb_execution_duration_seconds` | HistogramVec | `pair` | 套利全链路耗时：EVM Swap 发出 → HL 结果返回 |
| `evm_execution_gate_wait_seconds` | HistogramVec | `pair` | execution_gate 抢锁等待时间 |
| `hl_orderbook_latency_ms` | GaugeVec | `pair` | HL 订单簿推送延迟（服务端时间 → 本地收到，ms） |

---

## 三、告警规则建议

```yaml
# prometheus-alerts.yml
groups:
  - name: spread-circuit
    rules:
      # EVM 数据滞后（区块号长时间未更新）
      - alert: EvmBlockStale
        expr: changes(evm_latest_block_number[1m]) == 0
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "EVM 区块号最近一分钟没有更新，EvmWatcher WS 可能已断连"

      # 两端价格异常分叉（可能是数据问题）
      - alert: PriceAbnormalDivergence
        expr: |
          abs(evm_price_usdc{side="sell"} - hl_price_usdc{side="ask"})
          / hl_price_usdc{side="ask"}
          > 0.05
        for: 2m
        labels:
          severity: warning
        annotations:
          summary: "{{ $labels.pair }} 两端价格偏差超过 5%，检查数据源是否正常"

      # EVM Swap 连续失败
      - alert: EvmSwapHighFailRate
        expr: |
          sum by (pair) (rate(evm_swap_total{result="failed"}[5m]))
          / sum by (pair) (rate(evm_swap_total[5m]))
          > 0.3
        for: 2m
        labels:
          severity: warning
        annotations:
          summary: "{{ $labels.pair }} EVM Swap 失败率超过 30%，检查 RPC 连接或余额"

      # HL 下单全部失败
      - alert: LiquidOrderAllFailed
        expr: |
          sum by (pair) (rate(liquid_order_total{result="failed"}[5m])) > 0
          and sum by (pair) (rate(liquid_order_total{result="filled"}[5m])) == 0
        for: 3m
        labels:
          severity: critical
        annotations:
          summary: "{{ $labels.pair }} HL 下单持续失败，检查 HL API Key 和网络"

      # 订单簿延迟过高
      - alert: OrderBookHighLatency
        expr: hl_orderbook_latency_ms > 500
        for: 1m
        labels:
          severity: warning
        annotations:
          summary: "{{ $labels.pair }} HL 订单簿延迟 {{ $value }}ms，价格数据可能过时"

      # 套利全链路耗时 P95 过长
      - alert: ArbSlowExecution
        expr: |
          histogram_quantile(0.95, sum by (pair, le) (rate(arb_execution_duration_seconds_bucket[5m]))) > 5
        for: 2m
        labels:
          severity: warning
        annotations:
          summary: "{{ $labels.pair }} 套利 P95 耗时超过 5 秒，可能已错过机会窗口"
```

---

## 四、常见问题排查

| 现象 | 先看这个指标 | 可能原因 |
|------|-------------|---------|
| 价差一直高但没有触发 | `arb_price_diff_bps` vs `arb_min_profit_bps` | min_profit_bps 阈值设太高，或程序处于非 Idle 状态 |
| EVM 价格与 HL 价格严重偏离 | `evm_price_usdc` vs `hl_price_usdc` | EVM WS 断连后用了缓存旧价，检查 `evm_latest_block_number` 是否正常增长 |
| 区块号不增长 | `changes(evm_latest_block_number[1m])` | EvmWatcher WS 连接断开，等待自动重连或检查 WSS_RPC 节点 |
| Re-quote 放弃突然增多 | `evm_requote_aborted_total` | 触发到执行之间价格快速恶化、流动性不足或报价源波动 |
| Swap 频繁失败 | `evm_swap_total{result="failed"}` | 余额不足、RPC 超时、gas 价格飙升 |
| Swap 成功但 HL 下单失败 | `liquid_order_total{result="failed"}` | HL API 限频、签名错误、账户资金不足 |
| HL 总是挂单不成交 | `liquid_order_total{result="resting"}` | `limit_slippage` 设置太小，挂单价格偏离市场 |
| 链路耗时突然变长 | `arb_execution_duration_seconds` | HL 下单慢（网络）或 EVM 等块确认耗时增加 |
| 订单簿延迟高 | `hl_orderbook_latency_ms` | WebSocket 连接质量差，考虑切换 HL 节点 |
