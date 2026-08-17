use alloy::{
    primitives::{Address, Uint},
    providers::Provider,
    sol,
    transports::{BoxTransport, Transport},
};
use anyhow::Result;

use super::client::{
    build_http_provider, from_bigint, prepare_swap, settle_swap_receipt, swap_deadline, to_bigint,
    EthProvider, SwapOutcome, SwapRequest,
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
/// 与 Uniswap V3 的 swap_exact_input_single 区别仅在 Router 参数：无 fee 字段，有 deployer 字段
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
    let (amount_in_raw, amount_out_min_raw, swap_nonce) =
        prepare_swap(provider, req, nonce).await?;

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

    let receipt = IAlgebraRouter::new(req.router, provider)
        .exactInputSingle(params)
        .nonce(swap_nonce)
        .send()
        .await?
        .get_receipt()
        .await?;

    settle_swap_receipt(&receipt, req, "AlgebraClient")
}
