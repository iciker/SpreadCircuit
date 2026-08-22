# SpreadCircuit

[简体中文](README.md) | [English](README.en.md)

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](LICENSE)

SpreadCircuit is an experimental cross-venue spot arbitrage executor written in Rust. It watches HyperEVM AMMs and the HyperLiquid Spot order book, then sequentially executes an EVM swap and a HyperLiquid hedge when the spread covers fees, gas, limit-price slippage, and the configured profit buffer.

> [!WARNING]
> When `DRY_RUN=false`, SpreadCircuit uses a real private key and real funds. The two trade legs are not atomic: after the EVM transaction succeeds, the HyperLiquid order may still fail, fill partially, or time out. The affected pair is stopped and a `RecoveryRequired` record is written, but the application does not automatically repair the unhedged position. SpreadCircuit is not an official Hyperliquid project and does not guarantee profits.

## How it works

SpreadCircuit supports multiple trading pairs and maintains an independent arbitrage state machine for each pair.

| Direction | First leg on HyperEVM | Second leg on HyperLiquid |
|---|---|---|
| `buy_diff`, enabled by default | Sell the target token for USDC | Buy back the same amount of the target token |
| `sell_diff`, disabled by default | Spend USDC to buy the target token | Sell the actual token amount received on EVM |

Key conventions:

- `token0` is always HyperEVM USDC.
- `token1` is the target token, such as WHYPE or PURR.
- `{PAIR}_ORDER_AMOUNT` always means an amount of `token1`, not a USDC notional value.
- On every EVM block, all enabled DEX quote sources are queried concurrently and the best route is selected.
- Immediately before broadcasting a swap, the application requotes the actual order size and selects the route again.

Currently supported features:

- PRJX (Uniswap V3 compatible)
- HyperSwap (Uniswap V3 compatible)
- KittenSwap (Algebra Integral)
- Concurrent monitoring of multiple pairs
- Prometheus metrics and SQLite price records
- Trade recovery records and graceful shutdown

## Safety boundaries

The application includes the following safeguards:

- `DRY_RUN` defaults to `true`.
- Live trading additionally requires `LIVE_TRADING_ACK=I_UNDERSTAND_THE_RISK`.
- No trade is triggered when market data is stale, timestamped in the future, or too far apart between venues.
- The trigger threshold cannot be lower than the economic floor derived from fees, gas, profit buffer, and the worst allowed HyperLiquid limit price.
- The route is requoted using the real trade amount before EVM execution; the trade is abandoned if the price falls below the economic floor.
- `amountOutMinimum` cannot be lower than the economic break-even output.
- HyperLiquid market identity, base token, USDC quote token, and order precision are validated at startup.
- All wallet transactions are serialized through a global execution lock.
- If an EVM transaction has an uncertain outcome, or the HyperLiquid hedge fails or partially fills, the pair enters `RecoveryRequired`.
- Live trading refuses to start while unresolved recovery records exist.

These safeguards cannot eliminate:

- Directional exposure caused by non-atomic execution;
- RPC latency, disconnections, chain reorganizations, or exchange outages;
- Configuration errors, incorrect contract addresses, or unusual token behavior;
- Differences between actual gas/fees and configured estimates;
- Contract risk from unlimited ERC-20 router allowances;
- MEV, sudden liquidity loss, or extreme price movements.

## Quick start

### 1. Requirements

- Rust 1.81 or later
- A HyperEVM HTTP RPC endpoint
- A WebSocket RPC endpoint that supports HyperEVM new-block subscriptions
- A wallet that can access the HyperLiquid API and trade Spot markets
- The required assets on both HyperEVM and HyperCore/Spot

The official public Hyperliquid RPC currently does not expose Ethereum WebSocket JSON-RPC. `WSS_RPC` must support `eth_subscribe(newHeads)` and is different from the HyperLiquid market-data endpoint configured as `WSS_API`. See the self-hosted node section below for capability checks, and the [official HyperEVM documentation](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm) for network parameters.

### 2. Create the configuration

```bash
cp .env_example .env
```

At minimum, review these settings:

```dotenv
DRY_RUN=true
PRIVATE_KEY=your_private_key_without_the_0x_prefix

HTTPS_RPC=https://rpc.hyperliquid.xyz/evm
WSS_RPC=wss://a-provider-that-supports-hyperevm-subscriptions/evm
WSS_API=wss://api.hyperliquid.xyz/ws

PAIRS=HYPE
HYPE_ORDER_AMOUNT=0.6
HYPE_ASK_DIFF_PERCENT=36
HYPE_BID_DIFF_PERCENT=36
HYPE_ENABLE_SELL_ARB=false
```

`.env_example` contains the complete pair configuration, token precision, cost parameters, and DEX contract settings. Contract addresses and pool availability can change; verify them independently against official sources before use.

### 3. Build

```bash
cargo build --release
```

A successful build only confirms that the source and dependencies compile. It does not validate configuration, balances, liquidity, or execution routes.

### 4. Run in DRY_RUN mode

```bash
./target/release/spread-circuit
```

At startup, the application parses and validates its configuration before connecting to market-data and RPC services. It does not check balances; balances and allowances must be verified manually.

Confirm that the logs continue to contain messages equivalent to:

```text
[EvmWatcher] WebSocket connected
[EvmWatcher] New-block price
[ArbEngine] Spread detected
```

`DRY_RUN=true` does not submit trades, but the application still accesses RPC services, writes logs, and stores price records in SQLite.

### 5. Check metrics

The default metrics endpoint is `http://127.0.0.1:9090/metrics`:

```bash
curl http://127.0.0.1:9090/metrics | grep arb_
```

See the monitoring guide for Prometheus and Grafana setup, or directly import the provided [Grafana dashboard JSON](grafana-dashboard.json).

### 6. Enable live trading

Only consider live trading after an extended DRY_RUN period, small manual quote checks, and balance verification:

```dotenv
DRY_RUN=false
LIVE_TRADING_ACK=I_UNDERSTAND_THE_RISK
```

Before going live, verify at least the following:

- HyperEVM has enough native HYPE to pay gas.
- The target-token balance for `buy_diff`, and the USDC balance for `sell_diff`, are sufficient on HyperEVM.
- HyperLiquid has the USDC or target token required by the selected direction.
- `ORDER_AMOUNT` can be represented exactly at the HyperLiquid market precision.
- `LIQUID_FEE_BPS`, `GAS_COST_USDC`, and `PROFIT_BUFFER_BPS` match the current account and network conditions.
- Every router and quoter address has been independently verified.
- You understand the manual `RecoveryRequired` procedure.

## Running a self-hosted Hyperliquid non-validator node

Hyperliquid publishes a non-validator node that can provide a local HyperEVM HTTP JSON-RPC endpoint and reduce reliance on public RPC services. The following example targets Mainnet. Review the latest [official node repository](https://github.com/hyperliquid-dex/node) before installation.

> [!IMPORTANT]
> The official documentation currently specifies `http://localhost:3001/evm` as an HTTP endpoint; it does not promise WebSocket support on that port. SpreadCircuit calls `eth_subscribe(newHeads)` through `WSS_RPC`, so a self-hosted endpoint may be used for `WSS_RPC` only after that subscription has been tested successfully. A reverse proxy such as Nginx or Caddy can forward an existing WebSocket service, but cannot convert a plain HTTP RPC endpoint into a subscription service.

### 1. Prepare the server

The currently documented non-validator requirements are:

- Ubuntu 24.04;
- 16 vCPUs;
- 128 GB RAM;
- 500 GB SSD;
- Public TCP ports 4001 and 4002 for P2P gossip;
- For latency-sensitive workloads, a Tokyo region is preferred.

**Tested minimum configuration.** We have run this node for extended periods on a 4 vCPU / 8 GB RAM / 200 GB SSD machine with automatic data and log cleanup every 8 hours. It syncs reliably and keeps serving EVM RPC, which is sufficient for this project's quoting and execution. The official specification targets general-purpose non-validator use; when the node only serves as this project's RPC backend, the tested configuration is a reasonable starting point — scale up based on observed disk growth and memory pressure.

The node may produce approximately 100 GB of data and logs per day; a 200 GB disk fills in under two days without cleanup. A cleanup policy is what makes the smaller machine viable long-term. Run it via cron every 8 hours:

```bash
crontab -e
```

```cron
# Every 8 hours, delete node data and logs older than 8 hours (adjust the path to your data directory)
0 */8 * * * find ~/hl/data -type f -mmin +480 -delete 2>/dev/null; find ~/hl/data -type d -empty -delete 2>/dev/null
```

Cleanup only removes historical data files; the node keeps syncing and serving RPC, and `hl-visor` continues writing new blocks. After first bringing the node up, watch one or two cleanup cycles and confirm with `df -h` and `du -sh ~/hl/data` that disk usage settles within the expected range.

RPC port 3001 should not be exposed directly to the public internet. Remote access should use at least firewall allowlists, TLS, rate limiting, and access control.

### 2. Download and verify the official binary

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

Do not start the binary unless `gpg --verify` succeeds. `hl-visor` also verifies the `hl-node` binary it downloads and does not upgrade when signature verification fails.

### 3. Configure a Mainnet seed peer

The official Mainnet instructions require a non-validator node to configure at least one root peer. Query the current list first:

```bash
curl -sS -X POST \
  -H 'Content-Type: application/json' \
  -d '{"type":"gossipRootIps"}' \
  https://api.hyperliquid.xyz/info | jq .
```

Select a currently available IP from the response and write it to `~/override_gossip_config.json`. Do not keep a static IP copied from documentation indefinitely because the peer list can change.

```json
{
  "root_node_ips": [{ "Ip": "<ROOT_PEER_IP>" }],
  "try_new_peers": true,
  "chain": "Mainnet",
  "reserved_peer_ips": []
}
```

### 4. Start the EVM RPC service

```bash
~/hl-visor run-non-validator --serve-eth-rpc
```

Initial synchronization can take time. Repeated `applied block X` messages indicate that the node is receiving live blocks. Crash logs are stored under `~/hl/data/visor_child_stderr/`.

In another terminal, verify the local HTTP RPC:

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

On Mainnet, `eth_chainId` should return `0x3e7` (999), and the block number should keep increasing. SpreadCircuit can then use the local HTTP endpoint:

```dotenv
HTTPS_RPC=http://127.0.0.1:3001/evm
```

### 5. Verify whether the endpoint can be used for `WSS_RPC`

Use any Ethereum WebSocket client to connect to `ws://127.0.0.1:3001/evm`. For example, if `wscat` is installed:

```bash
wscat -c ws://127.0.0.1:3001/evm
```

After connecting, send:

```json
{"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":["newHeads"]}
```

Use the endpoint only if all of the following are true:

1. The server accepts the WebSocket upgrade.
2. The request returns a subscription ID rather than `method not found`.
3. New `eth_subscription` block notifications continue to arrive.
4. The subscription recovers after disconnecting and reconnecting.

If the test succeeds and SpreadCircuit runs on the same host:

```dotenv
WSS_RPC=ws://127.0.0.1:3001/evm
```

If the test fails, the official self-hosted node can still be used for `HTTPS_RPC`, but `WSS_RPC` must point to a third-party HyperEVM RPC service that explicitly supports `eth_subscribe(newHeads)`, or to a genuine Ethereum JSON-RPC WebSocket adapter that you operate. Do not set `WSS_RPC` to `wss://api.hyperliquid.xyz/ws`; that endpoint uses the HyperLiquid market-data protocol and belongs in `WSS_API`.

## Accounts and asset transfers

The same address can control both HyperEVM and HyperCore, but balances on the two systems are separate and must be checked independently.

| Location | Assets required by `buy_diff` | Assets required by `sell_diff` |
|---|---|---|
| HyperEVM | Target token and native HYPE for gas | USDC and native HYPE for gas |
| HyperLiquid Spot | USDC | Target token |

Use the `EVM <-> Core Transfer` feature in the Hyperliquid frontend to move supported linked assets. Only assets with an established Core/EVM linkage can be converted; do not assume that arbitrary same-named assets are interchangeable. Always make the first transfer with a small amount. See the [HyperCore and HyperEVM transfer documentation](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/hypercore-less-than-greater-than-hyperevm-transfers) for details.

When depositing from a centralized exchange, confirm that the exchange explicitly supports the intended destination network. HyperCore and HyperEVM are not the same deposit destination.

Hyperliquid currently accepts only native USDC for deposits from Arbitrum and applies a minimum deposit amount. Read the [Arbitrum USDC deposit guidance](https://hyperliquid.gitbook.io/hyperliquid-docs/support/faq/deposit-or-transfer-issues-missing-lost/deposited-via-arbitrum-network-usdc) before transferring.

SpreadCircuit does not currently include a balance-check command. Verify balances independently using a wallet, the Hyperliquid frontend, or trusted on-chain tools.

## Core configuration

Global settings:

| Variable | Required | Default | Description |
|---|---:|---:|---|
| `PRIVATE_KEY` | Yes | - | Private key used for EVM and HyperLiquid signing |
| `HTTPS_RPC` | Yes | - | HyperEVM HTTP RPC endpoint |
| `WSS_RPC` | Yes | - | HyperEVM WebSocket RPC supporting `eth_subscribe(newHeads)` |
| `WSS_API` | Yes | - | HyperLiquid WebSocket API |
| `PAIRS` | Yes | - | Comma-separated pair prefixes |
| `DRY_RUN` | No | `true` | Prevents real trades when enabled |
| `MIN_OUT_BPS` | No | `30` | EVM swap slippage protection |
| `LIMIT_SLIPPAGE` | No | `21` | HyperLiquid IOC limit-price offset |
| `LIQUID_FEE_BPS` | No | `1` | Estimated HyperLiquid fee |
| `GAS_COST_USDC` | No | `0.02` | Estimated gas cost per EVM transaction |
| `PROFIT_BUFFER_BPS` | No | `12` | Additional profit buffer |
| `DB_PATH` | No | `data.db` | SQLite database path |
| `METRICS_HOST` | No | `127.0.0.1` | Metrics listener address |
| `METRICS_PORT` | No | `9090` | Metrics listener port |

Every `{PAIR}` must define:

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

Optional pair settings:

```text
{PAIR}_SYMBOL
{PAIR}_ENABLE_SELL_ARB
{PAIR}_USE_HYPERSWAP
{PAIR}_USE_KITTEN
{PAIR}_MIN_OUT_BPS
{PAIR}_LIMIT_SLIPPAGE
```

PRJX and HyperSwap router/quoter addresses are required globally. `KITTEN_QUOTER` and `KITTEN_ROUTER` must either both be configured or both be omitted.

## Arbitrage conditions

### `buy_diff`

```text
hl_worst_buy = normalize(hl.ask * (1 + LIMIT_SLIPPAGE / 10000))

minimum_evm_usdc = ORDER_AMOUNT * hl_worst_buy
                   * (1 + (LIQUID_FEE_BPS + PROFIT_BUFFER_BPS) / 10000)
                   + GAS_COST_USDC

buy_diff = (evm.sell_price - hl.ask) / evm.sell_price * 10000
```

A trade is triggered only when both the configured threshold and the dynamic economic floor are satisfied. The first leg sells the fixed `ORDER_AMOUNT`; the second leg buys back the same amount on HyperLiquid.

### `sell_diff`

```text
evm_spend_usdc = ORDER_AMOUNT * evm.buy_price
hl_worst_sell = normalize(hl.bid * (1 - LIMIT_SLIPPAGE / 10000))

minimum_token1_out = (evm_spend_usdc * (1 + PROFIT_BUFFER_BPS / 10000)
                      + GAS_COST_USDC)
                     / (hl_worst_sell * (1 - LIQUID_FEE_BPS / 10000))

sell_diff = (hl.bid - evm.buy_price) / hl.bid * 10000
```

A trade is triggered only when `ENABLE_SELL_ARB=true` and both the configured threshold and dynamic economic floor are satisfied. The HyperLiquid hedge uses the EVM leg's actual output amount rather than an estimate.

## Architecture

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

- `EvmWatcher` and `OrderBookWatcher` are shared by all pairs.
- Each pair has an independent `ArbEngine`, `EvmTrader`, `LiquidTrader`, and typed channels.
- All `EvmTrader` instances share one wallet execution lock to prevent concurrent nonce conflicts.
- If a critical actor exits unexpectedly before normal shutdown, the application initiates a global stop.

## Recovery and shutdown

Trade stage, direction, EVM transaction hash, HyperLiquid order ID, and failure reason are written to the SQLite `trade_recovery` table.

```sql
SELECT * FROM trade_recovery;
```

When unresolved records exist:

- Live trading startup is rejected.
- DRY_RUN may start for quoting and diagnosis, but it still writes price records.
- EVM transactions, HyperLiquid orders, and balances on both venues must be checked manually.
- Recovery records should be cleared only after the position has actually been repaired.

After Ctrl+C, the application stops accepting new opportunities and waits up to 120 seconds for in-flight trades to finish. Remaining actors are terminated after the timeout, so recovery records must be checked again.

## Development and verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features --no-fail-fast
```

## Project structure

| Path | Purpose |
|---|---|
| `src/actors/arb_engine.rs` | Opportunity evaluation, two-leg state machine, and recovery control |
| `src/actors/evm_watcher.rs` | HyperEVM block monitoring and multi-DEX quoting |
| `src/actors/evm_trader.rs` | Live requoting, route selection, and swap execution |
| `src/actors/orderbook_watcher.rs` | HyperLiquid order-book subscription |
| `src/actors/liquid_trader.rs` | HyperLiquid IOC placement and cancellation |
| `src/db/mod.rs` | Price and recovery-record persistence |
| `src/config.rs` | Configuration parsing and safety validation |

## Current limitations

- Only HyperEVM USDC is supported as `token0`.
- EVM is always the first leg and HyperLiquid is always the second leg.
- There is no automatic inventory rebalancing or recovery tool.
- There is no built-in balance check, historical backtest, or profit-and-loss reporting.
- Gas and HyperLiquid fee estimates are manually configured.
- Router allowances are unlimited; assess whether to revoke them after disabling a router.
- The current version is `0.1.0`; validate it in DRY_RUN and with small amounts first.

## License

SpreadCircuit is licensed under the [GNU Affero General Public License v3.0](LICENSE). If you make a modified version available to users over a network, you must provide those users with the corresponding source code as required by AGPL-3.0.
