use alloy::{
    primitives::{Address, Uint},
    providers::Provider,
    sol,
    transports::{BoxTransport, Transport},
};
use anyhow::Result;

use super::client::{
    approve_if_needed, build_http_provider, from_bigint, parse_transfer_out, swap_deadline,
    to_bigint, to_bigint_ceil, EthProvider, SwapOutcome, SwapRequest, TRANSFER_EVENT_MISSING,
};

// Algebra Integral AMM Quoter 接口
// KittenSwap 使用此接口，与 Uniswap V3 QuoterV2 不兼容
// deployer 参数是 PoolKey 中的可选"盐额外键"，标准池传 address(0)
// PoolDeployer 合约地址已硬编码在 Quoter 合约内部，无需外部传入
sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IAlgebraQuoter {
        function quoteExactInputSingle(
            address tokenIn,
            address tokenOut,
            address deployer,
            uint256 amountIn,
            uint160 limitSqrtPrice
        ) external returns (
            uint256 amountOut,
            uint16 fee
        );

        function quoteExactOutputSingle(
            address tokenIn,
            address tokenOut,
            address deployer,
            uint256 amountOut,
            uint160 limitSqrtPrice
        ) external returns (
            uint256 amountIn,
            uint16 fee
        );
    }
}

// Algebra Integral AMM SwapRouter 接口
// 与 Uniswap V3 Router 的区别：无 fee 字段，有 deployer 字段
sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IAlgebraRouter {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            address deployer;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 limitSqrtPrice;
        }
        function exactInputSingle(ExactInputSingleParams calldata params)
            external payable returns (uint256 amountOut);
    }
}

/// KittenSwap（Algebra Integral AMM）报价客户端
/// 与 EvmClient 不同：无 fee tier，deployer 传 address(0) 匹配标准池
pub struct AlgebraQuoterClient {
    provider: EthProvider,
    quoter_addr: Address,
}

impl AlgebraQuoterClient {
    pub fn new(rpc_url: &str, quoter_addr: &str) -> Result<Self> {
        Self::from_provider(build_http_provider(rpc_url)?, quoter_addr)
    }

    pub fn from_provider(provider: EthProvider, quoter_addr: &str) -> Result<Self> {
        Ok(Self {
            provider,
            quoter_addr: quoter_addr.parse()?,
        })
    }

    fn quoter(&self) -> IAlgebraQuoter::IAlgebraQuoterInstance<BoxTransport, EthProvider> {
        IAlgebraQuoter::new(self.quoter_addr, self.provider.clone())
    }

    /// 精确输入报价：给定 amount_in，返回预期 amount_out（limitSqrtPrice=0 表示不限制）
    pub async fn quote_exact_input(
        &self,
        token_in: &str,
        token_out: &str,
        decimals_in: u8,
        decimals_out: u8,
        amount_in: f64,
    ) -> Result<f64> {
        let result = self
            .quoter()
            .quoteExactInputSingle(
                token_in.parse()?,
                token_out.parse()?,
                Address::ZERO, // 标准池 key.deployer=0
                to_bigint(amount_in, decimals_in)?,
                Uint::<160, 3>::ZERO,
            )
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
        amount_out: f64,
    ) -> Result<f64> {
        let result = self
            .quoter()
            .quoteExactOutputSingle(
                token_in.parse()?,
                token_out.parse()?,
                Address::ZERO, // 标准池 key.deployer=0
                to_bigint(amount_out, decimals_out)?,
                Uint::<160, 3>::ZERO,
            )
            .call()
            .await?;
        from_bigint(result.amountIn, decimals_in)
    }
}

/// KittenSwap（Algebra Integral AMM）交易执行函数
/// 与 Uniswap V3 的 swap_exact_input_single 功能相同，但使用 Algebra Router 接口
/// req.fee_tier 字段在 Algebra AMM 中不使用（由池内部决定）
/// nonce：调用方从链上 pending 状态显式获取，直接注入到 tx，绕过 alloy 本地缓存
pub async fn algebra_swap_exact_input_single<T, P>(
    provider: &P,
    req: &SwapRequest,
    nonce: u64,
) -> Result<SwapOutcome>
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

    let params = IAlgebraRouter::ExactInputSingleParams {
        tokenIn: req.token_in,
        tokenOut: req.token_out,
        deployer: Address::ZERO, // 标准池 deployer=0
        recipient: req.recipient,
        deadline: swap_deadline()?,
        amountIn: amount_in_raw,
        amountOutMinimum: amount_out_min_raw,
        limitSqrtPrice: Uint::<160, 3>::ZERO,
    };

    let router = IAlgebraRouter::new(req.router, provider);
    let receipt = router
        .exactInputSingle(params)
        .nonce(swap_nonce)
        .send()
        .await?
        .get_receipt()
        .await?;

    if !receipt.status() {
        anyhow::bail!("kitten swap tx 回滚: {:?}", receipt.transaction_hash);
    }

    // 从 receipt 日志中解析实际成交的 token_out 数量
    let actual_out = parse_transfer_out(receipt.inner.logs(), req.token_out, req.recipient)
        .ok_or_else(|| {
            tracing::error!(
                tx_hash = %receipt.transaction_hash,
                "[AlgebraClient] Transfer 事件未找到，链上已成功但实际金额未知"
            );
            anyhow::anyhow!("{TRANSFER_EVENT_MISSING}:{}", receipt.transaction_hash)
        })?;

    let amount_out = from_bigint(actual_out, req.decimals_out)?;
    tracing::info!(
        tx_hash = %receipt.transaction_hash,
        amount_in = req.amount_in,
        amount_out,
        "[AlgebraClient] kitten swap 交易确认"
    );
    Ok(SwapOutcome {
        amount_out,
        tx_hash: receipt.transaction_hash.to_string(),
    })
}
