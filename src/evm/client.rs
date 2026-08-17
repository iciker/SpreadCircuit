use alloy::{
    primitives::{Address, Uint, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
    sol,
    transports::{BoxTransport, Transport},
};
use anyhow::Result;
use std::sync::Arc;

/// swap 交易截止时间（秒）：套利对时效性极敏感，60 秒足够链上确认
pub const SWAP_DEADLINE_SECS: u64 = 60;

// 只定义我们需要的函数，用内联 Solidity 语法避免 ABI JSON struct 命名不确定性
sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IQuoterV2 {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint24 fee;
            uint160 sqrtPriceLimitX96;
        }
        struct QuoteExactOutputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amount;
            uint24 fee;
            uint160 sqrtPriceLimitX96;
        }
        function quoteExactInputSingle(QuoteExactInputSingleParams memory params)
            external
            returns (
                uint256 amountOut,
                uint160 sqrtPriceX96After,
                uint32 initializedTicksCrossed,
                uint256 gasEstimate
            );
        function quoteExactOutputSingle(QuoteExactOutputSingleParams memory params)
            external
            returns (
                uint256 amountIn,
                uint160 sqrtPriceX96After,
                uint32 initializedTicksCrossed,
                uint256 gasEstimate
            );
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        event Transfer(address indexed from, address indexed to, uint256 value);
    }
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface ISwapRouter01 {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        function exactInputSingle(ExactInputSingleParams calldata params)
            external payable returns (uint256 amountOut);
    }
}

/// swap 执行请求参数，避免 swap_exact_input_single 参数列表过长
pub struct SwapRequest {
    pub router: Address,
    pub token_in: Address,
    pub decimals_in: u8,
    pub token_out: Address,
    pub decimals_out: u8,
    /// Uniswap V3 fee tier（Algebra 路由不使用此字段，传 0 即可）
    pub fee_tier: u32,
    pub recipient: Address,
    pub amount_in: f64,
    pub amount_out_min: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwapOutcome {
    pub amount_out: f64,
    pub tx_hash: String,
}

/// 计算 swap 截止时间：当前 unix 秒 + SWAP_DEADLINE_SECS
pub fn swap_deadline() -> Result<U256> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    Ok(U256::from(now + SWAP_DEADLINE_SECS))
}

/// ERC20 余额查询（使用调用方已有的 provider，避免创建新连接）
pub async fn erc20_balance<T, P>(provider: &P, token: Address, wallet: Address) -> Result<U256>
where
    T: Transport + Clone,
    P: Provider<T>,
{
    Ok(IERC20::new(token, provider)
        .balanceOf(wallet)
        .call()
        .await?
        ._0)
}

/// 若授权额度不足则自动 approve MAX
/// 返回下一笔 tx 可用的 nonce（若发送了 approve tx 则为 nonce+1，否则原值不变）
pub async fn approve_if_needed<T, P>(
    provider: &P,
    token: Address,
    owner: Address,
    spender: Address,
    amount: U256,
    nonce: u64,
) -> Result<u64>
where
    T: Transport + Clone,
    P: Provider<T>,
{
    let erc20 = IERC20::new(token, provider);
    let allowance = erc20
        .allowance(owner, spender)
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("allowance 查询失败: {e}"))?
        ._0;

    if allowance >= amount {
        return Ok(nonce);
    }

    tracing::info!(token = %token, spender = %spender, "[EvmClient] ERC20 approve MAX");
    // 使用无限授权以避免每次 swap 都支付 approve gas（约 $0.01/次）
    // 风险已知：若 router 合约被攻击，钱包该 token 余额可被全部转走
    // 缓解措施：router 为知名 DEX 合约（HyperSwap/PRJX/KittenSwap Algebra），
    // 地址在 .env 中静态配置且启动时校验非零；三个 router 均获得无限授权，
    // 撤销需手动发送 approve(spender, 0)
    let receipt = erc20
        .approve(spender, U256::MAX)
        .nonce(nonce)
        .send()
        .await?
        .get_receipt()
        .await?;
    ensure_transaction_succeeded(
        receipt.status(),
        &receipt.transaction_hash.to_string(),
        "approve",
    )?;
    Ok(nonce + 1)
}

/// Receipt 已返回仍可能代表链上回滚，所有资金交易都通过这个检查统一 fail-closed。
pub fn ensure_transaction_succeeded(status: bool, tx_hash: &str, action: &str) -> Result<()> {
    anyhow::ensure!(status, "{action} tx 回滚: {tx_hash}");
    Ok(())
}

/// swap 公共前置：金额按精度缩放 + 按需 approve。
/// 返回 (amount_in_raw, amount_out_min_raw, swap_nonce)。
pub(crate) async fn prepare_swap<T, P>(
    provider: &P,
    req: &SwapRequest,
    nonce: u64,
) -> Result<(U256, U256, u64)>
where
    T: Transport + Clone,
    P: Provider<T>,
{
    let amount_in_raw = to_bigint(req.amount_in, req.decimals_in)?;
    let amount_out_min_raw = to_bigint_ceil(req.amount_out_min, req.decimals_out)?;
    let swap_nonce = approve_if_needed(
        provider,
        req.token_in,
        req.recipient,
        req.router,
        amount_in_raw,
        nonce,
    )
    .await?;
    Ok((amount_in_raw, amount_out_min_raw, swap_nonce))
}

/// swap 公共后置：回执 fail-closed 检查 + 从 Transfer 事件解析实际成交金额。
pub(crate) fn settle_swap_receipt(
    receipt: &alloy::rpc::types::TransactionReceipt,
    req: &SwapRequest,
    tag: &str,
) -> Result<SwapOutcome> {
    let tx_hash = receipt.transaction_hash.to_string();
    ensure_transaction_succeeded(receipt.status(), &tx_hash, "swap")?;

    // 从 receipt 日志中解析实际成交的 token_out 数量
    let actual_out = parse_transfer_out(receipt.inner.logs(), req.token_out, req.recipient)
        .ok_or_else(|| {
            tracing::error!(
                tx_hash = %receipt.transaction_hash,
                "[{tag}] Transfer 事件未找到，链上已成功但实际金额未知"
            );
            anyhow::Error::new(TransferEventMissing {
                tx_hash: receipt.transaction_hash.to_string(),
            })
        })?;

    let amount_out = from_bigint(actual_out, req.decimals_out)?;
    tracing::info!(
        tx_hash = %receipt.transaction_hash,
        amount_in = req.amount_in,
        amount_out,
        "[{tag}] swap 交易确认"
    );
    Ok(SwapOutcome {
        amount_out,
        tx_hash,
    })
}

/// 执行 exactInputSingle swap，返回实际换出的 token_out 数量
/// 步骤：approve（按需） → 发送交易 → 等待回执 → 解析 Transfer 事件
/// nonce：调用方从链上 pending 状态显式获取，直接注入到 tx，绕过 alloy 本地缓存
pub async fn swap_exact_input_single<T, P>(
    provider: &P,
    req: &SwapRequest,
    nonce: u64,
) -> Result<SwapOutcome>
where
    T: Transport + Clone,
    P: Provider<T>,
{
    let (amount_in_raw, amount_out_min_raw, swap_nonce) =
        prepare_swap(provider, req, nonce).await?;

    let params = ISwapRouter01::ExactInputSingleParams {
        tokenIn: req.token_in,
        tokenOut: req.token_out,
        fee: to_fee(req.fee_tier),
        recipient: req.recipient,
        deadline: swap_deadline()?,
        amountIn: amount_in_raw,
        amountOutMinimum: amount_out_min_raw,
        sqrtPriceLimitX96: Uint::<160, 3>::ZERO,
    };

    // 直接发送真实交易（alloy 泛型 Provider 路径下 CallBuilder::from() 不生效，
    // callStatic 会因 msg.sender=0x0 导致 STF，故跳过预验证直接执行）
    let receipt = ISwapRouter01::new(req.router, provider)
        .exactInputSingle(params)
        .nonce(swap_nonce)
        .send()
        .await?
        .get_receipt()
        .await?;

    settle_swap_receipt(&receipt, req, "EvmClient")
}

/// swap 链上成功但未找到 Transfer 事件：资金已划转、实际金额未知。
/// 类型化错误供调用方 downcast，避免跨模块字符串前缀契约。
#[derive(Debug)]
pub struct TransferEventMissing {
    pub tx_hash: String,
}

impl std::fmt::Display for TransferEventMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "transfer_event_missing:{}", self.tx_hash)
    }
}

impl std::error::Error for TransferEventMissing {}

/// 从 tx receipt 日志中提取 ERC20 Transfer(to=recipient) 的转账金额
/// swap 成功后用于获取实际成交的 token_out 数量
pub fn parse_transfer_out(
    logs: &[alloy::rpc::types::Log],
    token_out: Address,
    recipient: Address,
) -> Option<alloy::primitives::U256> {
    logs.iter()
        .filter(|log| log.address() == token_out)
        .find_map(|log| {
            let transfer = log.log_decode::<IERC20::Transfer>().ok()?;
            (transfer.inner.to == recipient).then_some(transfer.inner.value)
        })
}

/// RootProvider<BoxTransport> 实现 Provider；Arc<P> 自动实现 Provider，可安全跨 Actor 共享
pub type EthProvider = Arc<RootProvider<BoxTransport>>;

/// 构建 HTTP provider（RootProvider<BoxTransport>）
/// on_http() 返回带 fillers 的复杂类型；root().clone().boxed() 得到 RootProvider<BoxTransport>
pub fn build_http_provider(rpc_url: &str) -> Result<EthProvider> {
    Ok(Arc::new(
        ProviderBuilder::new()
            .on_http(rpc_url.parse()?)
            .root()
            .clone()
            .boxed(),
    ))
}

pub struct EvmClient {
    provider: EthProvider,
    quoter_addr: Address,
}

/// 将浮点数按精度缩放为链上整数
/// 使用整数运算避免 f64 * 10^18 的精度截断问题
/// 返回 Err 而非 panic，使调用方可以优雅处理异常输入（NaN/Infinity/超大值）
pub fn to_bigint(amount: f64, decimals: u8) -> Result<U256> {
    to_bigint_with_rounding(amount, decimals, false)
}

/// 最小输出必须向上取整，避免十进制转换把经济硬下限放宽一个最小单位。
pub fn to_bigint_ceil(amount: f64, decimals: u8) -> Result<U256> {
    to_bigint_with_rounding(amount, decimals, true)
}

fn to_bigint_with_rounding(amount: f64, decimals: u8, round_up: bool) -> Result<U256> {
    anyhow::ensure!(decimals <= 38, "to_bigint: decimals 不能超过 38");
    anyhow::ensure!(
        amount >= 0.0 && amount.is_finite(),
        "to_bigint: amount 必须为非负有限数，got {amount}"
    );
    // f64 整数部分的精确表示上限为 2^53 ≈ 9e15；超出此范围整数转换结果不可信
    anyhow::ensure!(
        amount < 1e15,
        "to_bigint: amount 超出安全精度范围（>1e15），got {amount}"
    );
    let floored = amount.floor();
    let int_part = floored as u128;
    let scale = 10u128.pow(decimals as u32);
    let scaled_fraction = (amount - floored) * scale as f64;
    let frac_scaled = if round_up {
        scaled_fraction.ceil() as u128
    } else {
        scaled_fraction.round() as u128
    };
    Ok(U256::from(int_part) * U256::from(scale) + U256::from(frac_scaled))
}

/// 将链上整数还原为浮点数
/// 返回 Err 而非 panic，使调用方可以优雅处理链上返回的异常大值
pub fn from_bigint(amount: U256, decimals: u8) -> Result<f64> {
    anyhow::ensure!(
        amount <= U256::from(u128::MAX),
        "from_bigint: amount 超出 u128 范围: {amount}"
    );
    Ok(amount.to::<u128>() as f64 / 10f64.powi(decimals as i32))
}

/// 将 u32 fee 转换为链上 uint24
fn to_fee(fee: u32) -> Uint<24, 1> {
    Uint::<24, 1>::from_limbs([fee as u64])
}

impl EvmClient {
    pub fn new(rpc_url: &str, quoter_addr: &str) -> Result<Self> {
        Self::from_provider(build_http_provider(rpc_url)?, quoter_addr)
    }

    pub fn from_provider(provider: EthProvider, quoter_addr: &str) -> Result<Self> {
        Ok(Self {
            provider,
            quoter_addr: quoter_addr.parse()?,
        })
    }

    fn quoter(&self) -> IQuoterV2::IQuoterV2Instance<BoxTransport, EthProvider> {
        IQuoterV2::new(self.quoter_addr, self.provider.clone())
    }

    /// 精确输入报价：给定 amount_in，返回预期 amount_out
    pub async fn quote_exact_input(
        &self,
        token_in: &str,
        token_out: &str,
        decimals_in: u8,
        decimals_out: u8,
        fee: u32,
        amount_in: f64,
    ) -> Result<f64> {
        let result = self
            .quoter()
            .quoteExactInputSingle(IQuoterV2::QuoteExactInputSingleParams {
                tokenIn: token_in.parse()?,
                tokenOut: token_out.parse()?,
                amountIn: to_bigint(amount_in, decimals_in)?,
                fee: to_fee(fee),
                sqrtPriceLimitX96: Uint::<160, 3>::ZERO,
            })
            .call()
            .await?;

        from_bigint(result.amountOut, decimals_out)
    }

    /// 精确输出报价：给定 amount_out，返回需要的 amount_in
    pub async fn quote_exact_output(
        &self,
        token_in: &str,
        token_out: &str,
        decimals_in: u8,
        decimals_out: u8,
        fee: u32,
        amount_out: f64,
    ) -> Result<f64> {
        let result = self
            .quoter()
            .quoteExactOutputSingle(IQuoterV2::QuoteExactOutputSingleParams {
                tokenIn: token_in.parse()?,
                tokenOut: token_out.parse()?,
                amount: to_bigint(amount_out, decimals_out)?,
                fee: to_fee(fee),
                sqrtPriceLimitX96: Uint::<160, 3>::ZERO,
            })
            .call()
            .await?;

        from_bigint(result.amountIn, decimals_in)
    }

    /// ERC20 余额查询
    pub async fn get_balance(&self, token: &str, wallet: Address) -> Result<U256> {
        erc20_balance(&*self.provider, token.parse()?, wallet).await
    }

    /// ERC20 授权额度查询
    pub async fn get_allowance(
        &self,
        token: &str,
        owner: Address,
        spender: Address,
    ) -> Result<U256> {
        let erc20 = IERC20::new(token.parse()?, self.provider.clone());
        Ok(erc20.allowance(owner, spender).call().await?._0)
    }
}
