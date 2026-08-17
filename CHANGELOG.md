# Changelog

所有重要变更记录在此文件中，格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

---

## [Unreleased]

### Fixed

- 经济门槛现在覆盖 HL 最坏限价、手续费、gas 和利润缓冲；EVM `amountOutMinimum` 不再低于盈亏平衡输出。
- 拒绝 NaN、无穷、非正数和交叉订单簿行情。
- ERC20 approve 回执现在检查链上执行状态；大精度金额使用 U256 乘法避免 u128 溢出。
- HL 下单按照 spot metadata 校验资产映射并规格化价格、数量精度。
- 退出时等待在途交易完成；交易恢复阶段、tx hash 和 oid 持久化到 SQLite，未恢复记录会阻止实盘重启。
- SQLite 价格记录增加最大行数保留策略，连续写入失败会触发安全关闭。

## [0.4.0] - 2026-03-23

### Fixed

#### EvmTrader 执行前 re-quote，修复链上 STF 频繁回滚

**根本原因（两重叠加）：**

1. **基准金额不一致**：`EvmWatcher` 用 `QUOTE_AMOUNT=100` USDC 报价，而实际执行金额由 `order_amount` 决定，两者的价格冲击不同，导致 `amountOutMinimum` 存在系统性误差。
2. **价格陈旧**：报价在块发布时完成，到 swap tx 上链至少经过 1-3 秒（1-3 个块），期间价格移动超过 `min_out_bps`（0.08%）即触发 STF 回滚。

**修复：** `EvmTrader` 在发送 tx 前增加实时 re-quote 步骤：

```
旧流程：用 target_amount（陈旧价格）→ 发 swap tx → 频繁 STF 回滚
新流程：re-quote（实际金额 + 当前链状态）
         ├─ fresh_out < target_amount？→ 放弃，不发 tx，省 gas
         └─ fresh_out ≥ target_amount？→ amountOutMinimum 取
                                          max(fresh_out × (1 - min_out_bps), target_amount)
                                          → 发 swap tx
```

`target_amount` 是覆盖 HL 最坏限价、手续费、gas 和利润缓冲的经济硬下限，同时也是 `amountOutMinimum` 的下界。

### Added

- **`evm_requote_aborted_total` 指标**（`metrics.rs`）：统计 re-quote 后因价格恶化放弃的次数，与 `evm_swap_total{result="failed"}`（链上回滚）区分，便于监控套利机会质量。

### Refactored

- **`BPS` 常量去重**（`actors/mod.rs`）：从 `arb_engine.rs` 和 `evm_trader.rs` 各自独立定义，提升到 `actors/mod.rs` 统一维护。
- **`evm_trader.rs` 代码整洁度**：
  - 提取 `build_quoters()` 命名函数，替代立即调用匿名闭包（IIFE），提升可读性
  - 引入 `PRICE_DETERIORATED` 常量，并在 doc 注释中声明 `starts_with` 匹配约定，防止维护者修改 `bail!` 消息时静默破坏匹配逻辑
  - `requote()` 函数参数从 10 个精简为 7 个（内部推导 token 方向，不再依赖外部传入）
  - 调换 requote 与 balance/nonce 查询顺序：re-quote 失败（价格恶化）时提前退出，节省两次 RPC 往返

---

## [0.3.x] - 2026-03-03 至 2026-03-17

历史变更见 git log。
