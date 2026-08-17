# 当前使用的 DEX 合约

下表只记录程序和 `.env_example` 当前使用的 HyperEVM 地址；配置仍是运行时的唯一来源。

| DEX | 用途 | 地址 |
|-----|------|------|
| HyperSwap V3 | SwapRouter01 | `0x4e2960a8cd19b467b82d26d83facb0fae26b094d` |
| HyperSwap V3 | QuoterV2 | `0x03A918028f22D9E1473B7959C927AD7425A45C7C` |
| PRJX | Swap Router | `0x1EbDFC75FfE3ba3de61E7138a3E8706aC841Af9B` |
| PRJX | Quoter | `0x239F11a7A3E08f2B8110D4CA9F6B95d4c8865258` |
| KittenSwap | Algebra Quoter | `0x3aA96eDb755C44F3E50C5408a36abb52f28326Ba` |
| KittenSwap | Algebra Router | `0x4e73E421480a7E0C24fB3c11019254edE194f736` |

来源：

- PRJX: <https://prjxdocs.notion.site/>
- HyperSwap V3: <https://docs.hyperswap.exchange/hyperswap/hyperswap-amm/contracts/or-hyper-evm/v3>
- KittenSwap: <https://kittenswap.finance/>

合约地址可能迁移。实盘前应再次对照项目官方资料，并先运行
首次实盘前，应通过 DEX 官方前端或区块浏览器核对池、Quoter 和 Router 状态，并使用独立钱包做小额执行验证。
