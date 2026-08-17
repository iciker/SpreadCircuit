# 动态阈值：后续工作

当前实现只使用静态配置和实时名义金额计算盈亏平衡线：

```text
effective_bps = max(configured_bps,
                    liquid_fee_bps + profit_buffer_bps
                    + gas_cost_usdc / order_notional_usdc * 10000)
```

本文档描述的动态策略均未实现。若继续演进，建议按以下顺序拆成独立变更，
每项先在 `DRY_RUN=true` 下观测至少 24 小时，再考虑实盘：

1. 余额驱动：token1 占比偏高时适度降低 sell 阈值，偏低时提高阈值。
2. Gas 感知：从链上估算当前交易成本，替换静态 `GAS_COST_USDC`。
3. 市场分布：使用价差滚动窗口调整阈值，仅作为低优先级实验。

所有方案必须满足：

- 设置明确的 floor/ceiling，动态偏移不能无限扩大。
- 数据查询失败时退回当前静态阈值。
- 暴露实际生效阈值、余额比例或 Gas 成本指标。
- 保留单笔盈亏平衡线，动态策略不能把阈值降到成本线以下。
- 在 `tests/` 添加边界、失败回退和限幅测试，不在生产模块内写测试。

优先评估的配置草案：`SELL_BPS_DELTA`、`BALANCE_TARGET_RATIO`、
`BALANCE_MIN_RATIO`、`BALANCE_MAX_RATIO`。这些名称尚未成为配置契约。
