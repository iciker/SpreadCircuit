# SpreadCircuit

[简体中文](README.md) | [English](README.en.md)

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](LICENSE)

SpreadCircuit 是一个用 Rust 编写的实验性跨场所现货套利执行器。它同时监听 HyperEVM AMM 和 HyperLiquid Spot 订单簿，在价差覆盖手续费、Gas、限价偏移和利润缓冲后，依次执行 EVM Swap 与 HyperLiquid 对冲。

> [!WARNING]
> SpreadCircuit 会在 `DRY_RUN=false` 时使用真实私钥和真实资金。两条交易腿不是原子执行：EVM 成功后，HyperLiquid 下单仍可能失败、部分成交或超时。程序会停止对应交易对并记录 `RecoveryRequired`，但不会自动修复未对冲仓位。它不是 Hyperliquid 官方项目，也不保证盈利。

## 工作方式

SpreadCircuit 支持多个交易对，并为每个交易对独立维护套利状态机。

| 方向 | HyperEVM 第一腿 | HyperLiquid 第二腿 |
|---|---|---|
| `buy_diff`，默认开启 | 卖出目标代币，获得 USDC | 买入相同数量的目标代币 |
| `sell_diff`，默认关闭 | 使用 USDC 买入目标代币 | 卖出 EVM 实际获得的目标代币 |

其中：

- `token0` 固定为 HyperEVM USDC。
- `token1` 是目标代币，例如 WHYPE、PURR。
- `{PAIR}_ORDER_AMOUNT` 始终表示 `token1` 数量，不是 USDC 名义金额。
- 每个 EVM 区块会并行请求所有已启用 DEX，并选取最优报价。
- 真正发送 Swap 前会按实际下单量重新报价和重新选择路由。

当前支持：

- PRJX（Uniswap V3 兼容）
- HyperSwap（Uniswap V3 兼容）
- KittenSwap（Algebra Integral）
- 多交易对并行监控
- Prometheus 指标和 SQLite 价格记录
- 交易恢复日志和优雅关闭

## 安全边界

程序已经实现以下保护：

- `DRY_RUN` 默认为 `true`。
- 实盘必须额外设置 `LIVE_TRADING_ACK=I_UNDERSTAND_THE_RISK`。
- 市场数据过期、来自未来或时间偏差过大时不触发交易。
- 触发门槛不会低于手续费、Gas、利润缓冲和最坏 HL 限价共同决定的经济下限。
- EVM 执行前按真实金额重新报价；价格跌破经济下限时放弃交易。
- `amountOutMinimum` 不会低于经济盈亏平衡输出。
- HyperLiquid 市场、base token、USDC quote 和下单精度在启动时验证。
- 所有钱包交易通过全局执行锁串行化。
- EVM 已提交但结果不确定、HL 对冲失败或部分成交时进入 `RecoveryRequired`。
- 实盘启动时如果存在未解决恢复记录，程序拒绝继续交易。

这些保护不能消除：

- 两腿非原子执行造成的方向敞口；
- RPC 延迟、断线、链重组或交易所不可用；
- 配置错误、错误合约地址或异常代币行为；
- 实际 Gas、手续费与配置估值不一致；
- Router 无限 ERC-20 allowance 的合约风险；
- MEV、流动性骤降和极端价格跳变。

## 快速开始

### 1. 环境要求

- Rust 1.81 或更高版本
- HyperEVM HTTP RPC
- 支持 HyperEVM 新块订阅的 WebSocket RPC
- 可用于 HyperLiquid API 和 Spot 交易的钱包
- 分别准备 HyperEVM 与 HyperCore/Spot 侧所需资产

Hyperliquid 官方公共 RPC 当前不提供 WebSocket JSON-RPC。`WSS_RPC` 必须支持 Ethereum JSON-RPC 的 `eth_subscribe(newHeads)`；它不同于 HyperLiquid 行情 API 的 `WSS_API`。自建节点的能力边界和验证方法见下文，网络参数见 [HyperEVM 官方文档](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm)。

### 2. 创建配置

```bash
cp .env_example .env
```

至少检查以下配置：

```dotenv
DRY_RUN=true
PRIVATE_KEY=你的私钥，不含0x前缀

HTTPS_RPC=https://rpc.hyperliquid.xyz/evm
WSS_RPC=wss://支持HyperEVM订阅的节点/evm
WSS_API=wss://api.hyperliquid.xyz/ws

PAIRS=HYPE
HYPE_ORDER_AMOUNT=0.6
HYPE_ASK_DIFF_PERCENT=36
HYPE_BID_DIFF_PERCENT=36
HYPE_ENABLE_SELL_ARB=false
```

`.env_example` 已包含完整的交易对、代币精度、成本参数和 DEX 合约配置。合约地址和池可用性可能变化，使用前应对照官方来源和 [DEX 地址说明](docs/quote,路由地址和官方网站.md) 独立确认。

### 3. 编译

```bash
cargo build --release
```

编译成功只说明源码和依赖可用，不代表配置、余额、流动性或交易路径已经验证。

### 4. 运行 DRY_RUN

```bash
./target/release/spread-circuit
```

程序启动时会先解析并验证配置，然后连接行情与 RPC。它不会检查两侧余额；余额和授权必须人工核对。

确认日志中持续出现：

```text
[EvmWatcher] WebSocket 已连接
[EvmWatcher] 新块价格
[ArbEngine] 检测差价
```

`DRY_RUN=true` 不会发送交易，但仍会访问 RPC、写入日志，并将价格记录保存到 SQLite。

### 5. 检查指标

默认指标地址为 `http://127.0.0.1:9090/metrics`：

```bash
curl http://127.0.0.1:9090/metrics | grep arb_
```

Grafana 配置说明见 [监控文档](docs/grafana-dashboard.md)，也可以直接导入 [Dashboard JSON](docs/grafana-dashboard.json)。

### 6. 启用实盘

只有在完成长时间 DRY_RUN、小额人工报价验证和余额核对后，才考虑启用：

```dotenv
DRY_RUN=false
LIVE_TRADING_ACK=I_UNDERSTAND_THE_RISK
```

实盘前至少确认：

- HyperEVM 有足够的原生 HYPE 支付 Gas。
- `buy_diff` 使用的目标代币和 `sell_diff` 使用的 USDC 余额充足。
- HyperLiquid 侧有对应方向所需的 USDC 或目标代币。
- `ORDER_AMOUNT` 可以被 HyperLiquid 市场精度准确表示。
- `LIQUID_FEE_BPS`、`GAS_COST_USDC` 和 `PROFIT_BUFFER_BPS` 符合当前账户与网络状况。
- 每个 Router 和 Quoter 地址都已独立核对。
- 已经了解 `RecoveryRequired` 的人工恢复流程。

## 自建 Hyperliquid 非验证节点

Hyperliquid 官方提供非验证节点程序。它可以在本机提供 HyperEVM HTTP JSON-RPC，减少对公共 RPC 的依赖。以下步骤以 Mainnet 为例，执行前应再次阅读最新的 [官方节点仓库](https://github.com/hyperliquid-dex/node)。

> [!IMPORTANT]
> 官方文档目前明确记录的是 `http://localhost:3001/evm` HTTP RPC，没有承诺该端口支持 WebSocket。SpreadCircuit 的 `WSS_RPC` 会调用 `eth_subscribe(newHeads)`；只有实际测试成功后才能把自建端点填入 `WSS_RPC`。Nginx、Caddy 等反向代理只能转发已有的 WebSocket，不能把普通 HTTP RPC 自动变成订阅服务。

### 1. 准备服务器

官方当前给出的非验证节点要求是：

- Ubuntu 24.04；
- 16 vCPU；
- 128 GB RAM；
- 500 GB SSD；
- 公网开放 TCP 4001 和 4002，用于 P2P gossip；
- 低延迟场景优先选择日本东京区域。

节点默认可能产生约 100 GB/天的数据和日志。必须配置磁盘监控、归档或清理策略。RPC 端口 3001 不应直接暴露到公网；远程访问时至少使用防火墙白名单、TLS、限流和访问控制。

### 2. 下载并验证官方程序

```bash
sudo apt update
sudo apt install -y curl git gnupg jq

git clone --depth 1 https://github.com/hyperliquid-dex/node.git ~/hyperliquid-node
gpg --import ~/hyperliquid-node/pub_key.asc

echo '{"chain":"Mainnet"}' > ~/visor.json
curl -fL https://binaries.hyperliquid.xyz/Mainnet/hl-visor -o ~/hl-visor
curl -fL https://binaries.hyperliquid.xyz/Mainnet/hl-visor.asc -o ~/hl-visor.asc
gpg --verify ~/hl-visor.asc ~/hl-visor
chmod 0755 ~/hl-visor
```

必须确认 `gpg --verify` 成功后再启动。`hl-visor` 会继续校验它下载的 `hl-node`，签名校验失败时不会升级。

### 3. 配置 Mainnet seed peer

官方要求 Mainnet 非验证节点至少配置一个 root peer。先查询当前列表：

```bash
curl -sS -X POST \
  -H 'Content-Type: application/json' \
  -d '{"type":"gossipRootIps"}' \
  https://api.hyperliquid.xyz/info | jq .
```

从响应中选择当前可用的 IP，写入 `~/override_gossip_config.json`。不要长期复制 README 中的静态 IP，peer 列表可能变化。

```json
{
  "root_node_ips": [{ "Ip": "<ROOT_PEER_IP>" }],
  "try_new_peers": true,
  "chain": "Mainnet",
  "reserved_peer_ips": []
}
```

### 4. 启动 EVM RPC

```bash
~/hl-visor run-non-validator --serve-eth-rpc
```

初次同步可能需要一段时间。持续出现 `applied block X` 才表示节点正在接收实时区块。崩溃日志位于 `~/hl/data/visor_child_stderr/`。

另开终端验证本地 HTTP RPC：

```bash
curl -sS -X POST \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' \
  http://127.0.0.1:3001/evm

curl -sS -X POST \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
  http://127.0.0.1:3001/evm
```

Mainnet 的 `eth_chainId` 应返回 `0x3e7`（999），区块号应持续增长。随后可以让 SpreadCircuit 使用本机 HTTP RPC：

```dotenv
HTTPS_RPC=http://127.0.0.1:3001/evm
```

### 5. 验证是否能用于 `WSS_RPC`

使用任意 Ethereum WebSocket 客户端连接 `ws://127.0.0.1:3001/evm`。例如已经安装 `wscat` 时：

```bash
wscat -c ws://127.0.0.1:3001/evm
```

连接后发送：

```json
{"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":["newHeads"]}
```

只有同时满足以下条件才能使用这个端点：

1. 服务器接受 WebSocket Upgrade；
2. 请求返回 subscription ID，而不是 `method not found`；
3. 后续持续收到 `eth_subscription` 新区块通知；
4. 断开重连后仍能恢复订阅。

测试通过时，同机运行可配置：

```dotenv
WSS_RPC=ws://127.0.0.1:3001/evm
```

如果测试失败，自建官方节点仍可用于 `HTTPS_RPC`，但 `WSS_RPC` 仍需使用明确支持 `eth_subscribe(newHeads)` 的第三方 HyperEVM RPC，或自行实现真正的 Ethereum JSON-RPC WebSocket 订阅适配层。不要把 `wss://api.hyperliquid.xyz/ws` 填入 `WSS_RPC`；它属于 HyperLiquid 行情协议，应继续配置为 `WSS_API`。

## 账户与资产转移

同一个地址可以控制 HyperEVM 和 HyperCore，但两侧余额是不同的账本状态，需要分别核对。

| 位置 | `buy_diff` 所需资产 | `sell_diff` 所需资产 |
|---|---|---|
| HyperEVM | 目标代币和原生 HYPE Gas | USDC 和原生 HYPE Gas |
| HyperLiquid Spot | USDC | 目标代币 |

建议通过 Hyperliquid 前端的 `EVM <-> Core Transfer` 功能移动已关联资产。只有已经完成 Core/EVM 关联的资产才能转换，不应假设任意同名资产都能互换。第一次操作必须使用小额测试，具体规则见 [HyperCore 与 HyperEVM 转账文档](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/hypercore-less-than-greater-than-hyperevm-transfers)。

如果从中心化交易所充值，必须确认交易所明确支持目标网络。HyperCore 与 HyperEVM 不是同一个充值目标。

通过 Arbitrum 向 Hyperliquid 充值时，官方当前仅支持原生 USDC，并存在最低充值金额。操作前阅读 [Arbitrum USDC 充值说明](https://hyperliquid.gitbook.io/hyperliquid-docs/support/faq/deposit-or-transfer-issues-missing-lost/deposited-via-arbitrum-network-usdc)。

SpreadCircuit 当前没有余额检查命令，余额必须通过钱包、Hyperliquid 前端或经过验证的链上工具独立确认。

## 核心配置

全局配置：

| 变量 | 必需 | 默认值 | 说明 |
|---|---:|---:|---|
| `PRIVATE_KEY` | 是 | - | EVM 和 HyperLiquid 签名私钥 |
| `HTTPS_RPC` | 是 | - | HyperEVM HTTP RPC |
| `WSS_RPC` | 是 | - | 支持 `eth_subscribe(newHeads)` 的 HyperEVM WebSocket RPC |
| `WSS_API` | 是 | - | HyperLiquid WebSocket API |
| `PAIRS` | 是 | - | 交易对前缀，逗号分隔 |
| `DRY_RUN` | 否 | `true` | 是否禁止真实交易 |
| `MIN_OUT_BPS` | 否 | `30` | EVM Swap 滑点保护 |
| `LIMIT_SLIPPAGE` | 否 | `21` | HyperLiquid IOC 限价偏移 |
| `LIQUID_FEE_BPS` | 否 | `1` | 估算的 HL 手续费 |
| `GAS_COST_USDC` | 否 | `0.02` | 单次 EVM 交易 Gas 估值 |
| `PROFIT_BUFFER_BPS` | 否 | `12` | 额外利润缓冲 |
| `DB_PATH` | 否 | `data.db` | SQLite 路径 |
| `METRICS_HOST` | 否 | `127.0.0.1` | 指标监听地址 |
| `METRICS_PORT` | 否 | `9090` | 指标端口 |

每个 `{PAIR}` 必须配置：

```text
{PAIR}_TOKEN0
{PAIR}_DECIMALS0
{PAIR}_TOKEN1
{PAIR}_DECIMALS1
{PAIR}_SPOT_COIN
{PAIR}_ORDER_AMOUNT
{PAIR}_ASK_DIFF_PERCENT
{PAIR}_BID_DIFF_PERCENT
{PAIR}_FEE_TIER
```

可选的交易对配置：

```text
{PAIR}_SYMBOL
{PAIR}_ENABLE_SELL_ARB
{PAIR}_USE_HYPERSWAP
{PAIR}_USE_KITTEN
{PAIR}_MIN_OUT_BPS
{PAIR}_LIMIT_SLIPPAGE
```

PRJX 和 HyperSwap Router/Quoter 地址是全局必需配置。KittenSwap 的 `KITTEN_QUOTER` 与 `KITTEN_ROUTER` 必须同时配置或同时留空。

## 套利条件

### `buy_diff`

```text
hl_worst_buy = normalize(hl.ask * (1 + LIMIT_SLIPPAGE / 10000))

minimum_evm_usdc = ORDER_AMOUNT * hl_worst_buy
                   * (1 + (LIQUID_FEE_BPS + PROFIT_BUFFER_BPS) / 10000)
                   + GAS_COST_USDC

buy_diff = (evm.sell_price - hl.ask) / evm.sell_price * 10000
```

只有配置门槛与动态经济下限都满足时才触发。第一腿卖出固定的 `ORDER_AMOUNT`，第二腿在 HyperLiquid 买回同样数量。

### `sell_diff`

```text
evm_spend_usdc = ORDER_AMOUNT * evm.buy_price
hl_worst_sell = normalize(hl.bid * (1 - LIMIT_SLIPPAGE / 10000))

minimum_token1_out = (evm_spend_usdc * (1 + PROFIT_BUFFER_BPS / 10000)
                      + GAS_COST_USDC)
                     / (hl_worst_sell * (1 - LIQUID_FEE_BPS / 10000))

sell_diff = (hl.bid - evm.buy_price) / hl.bid * 10000
```

只有 `ENABLE_SELL_ARB=true` 且配置门槛与动态经济下限都满足时才触发。HyperLiquid 第二腿对冲 EVM 第一腿的实际输出，不使用估算数量。

## 架构

```text
EvmWatcher ─────── broadcast<EvmPrice> ───────┐
                                               v
OrderBookWatcher ─ broadcast<OrderBook> ─> ArbEngine
                                               |
                        +----------------------+
                        | EvmSwapRequest       | LiquidCommand
                        v                      v
                    EvmTrader             LiquidTrader
                        |                      |
                        +---- TradeResult -----+

ArbEngine ─ broadcast<PriceRecord> ─> PriceDB ─> SQLite
MetricsServer ─> Prometheus / Grafana
```

- `EvmWatcher` 和 `OrderBookWatcher` 由所有交易对共享。
- 每个交易对拥有独立的 `ArbEngine`、`EvmTrader`、`LiquidTrader` 和类型化 channel。
- 所有 `EvmTrader` 共享一个钱包执行锁，避免并发 nonce 冲突。
- 任一关键 actor 在正常关闭前意外退出，会触发全局停止。

## 故障恢复与关闭

交易阶段、方向、EVM tx hash、HyperLiquid oid 和失败原因会写入 SQLite 的 `trade_recovery` 表。

```sql
SELECT * FROM trade_recovery;
```

如果存在未解决记录：

- 实盘启动会被拒绝；
- DRY_RUN 可以启动，用于报价和排查，但仍会写价格记录；
- 必须人工核对 EVM 交易、HyperLiquid 订单和两侧余额；
- 仓位真正修复后，才能清除对应交易对的恢复记录。

收到 Ctrl+C 后，程序停止接收新机会，并最多等待 120 秒完成在途交易。超时后会终止剩余 actor，此时必须再次检查恢复记录。

## 开发与验证

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features --no-fail-fast
```

公开仓库仅分发生产代码和正式运行文档；内部测试与诊断工具不随仓库发布。

## 项目结构

| 路径 | 作用 |
|---|---|
| `src/actors/arb_engine.rs` | 机会评估、两腿状态机和恢复控制 |
| `src/actors/evm_watcher.rs` | HyperEVM 区块监听和多 DEX 报价 |
| `src/actors/evm_trader.rs` | 实时重报价、路由选择和 Swap 执行 |
| `src/actors/orderbook_watcher.rs` | HyperLiquid 订单簿订阅 |
| `src/actors/liquid_trader.rs` | HyperLiquid IOC 下单和撤单 |
| `src/db/mod.rs` | 价格与恢复记录持久化 |
| `src/config.rs` | 配置解析和安全验证 |

## 当前限制

- 只支持 HyperEVM USDC 作为 `token0`。
- EVM 永远是第一腿，HyperLiquid 永远是第二腿。
- 没有自动库存再平衡或自动恢复工具。
- 没有内置余额检查、历史回测或收益统计。
- Gas 和 HyperLiquid 手续费依赖人工配置估值。
- Router allowance 使用无限授权，停用后应自行评估是否撤销。
- 当前版本为 `0.1.0`，应先以 DRY_RUN 和小额资金验证。

## 许可证

SpreadCircuit 根据 [GNU Affero General Public License v3.0](LICENSE) 发布。通过网络向用户提供修改版本时，必须按照 AGPL-3.0 的要求向这些用户提供对应源代码。
