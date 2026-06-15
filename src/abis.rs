use ethers::prelude::abigen;

// 只生成机器人当前会用到的 ERC-20 方法，减少 ABI 面积。
abigen!(
    IERC20,
    r#"[
        function decimals() external view returns (uint8)
        function symbol() external view returns (string)
        function balanceOf(address account) external view returns (uint256)
        function allowance(address owner, address spender) external view returns (uint256)
        function approve(address spender, uint256 amount) external returns (bool)
    ]"#,
);

// V2 报价只需要 pair 的 token 顺序和 reserves。
abigen!(
    IUniswapV2Pair,
    r#"[
        function token0() external view returns (address)
        function token1() external view returns (address)
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast)
    ]"#,
);

// V3 报价通过 Quoter 的 eth_call 完成，不会发送链上交易。
abigen!(
    IUniswapV3Quoter,
    r#"[
        function quoteExactInputSingle(address tokenIn, address tokenOut, uint24 fee, uint256 amountIn, uint160 sqrtPriceLimitX96) external returns (uint256 amountOut)
    ]"#,
);
