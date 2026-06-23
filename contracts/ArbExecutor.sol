// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IERC20 {
    function balanceOf(address account) external view returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function approve(address spender, uint256 amount) external returns (bool);
}

// Uniswap V2 Router 的最小接口，只保留两 token path 的精确输入 swap。
interface IUniswapV2Router02 {
    function swapExactTokensForTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        address[] calldata path,
        address to,
        uint256 deadline
    ) external returns (uint256[] memory amounts);
}

// Uniswap V3 SwapRouter 的最小接口。
interface ISwapRouter {
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
        external
        payable
        returns (uint256 amountOut);
}

// Balancer V2 Vault 的最小接口。
interface IBalancerV2Vault {
    enum SwapKind {
        GIVEN_IN,
        GIVEN_OUT
    }

    struct BatchSwapStep {
        bytes32 poolId;
        uint256 assetInIndex;
        uint256 assetOutIndex;
        uint256 amount;
        bytes userData;
    }

    struct FundManagement {
        address sender;
        bool fromInternalBalance;
        address payable recipient;
        bool toInternalBalance;
    }

    function batchSwap(
        SwapKind kind,
        BatchSwapStep[] calldata swaps,
        address[] calldata assets,
        FundManagement calldata funds,
        int256[] calldata limits,
        uint256 deadline
    ) external payable returns (int256[] memory assetDeltas);
}

contract ArbExecutor {
    // 必须和 Rust 里的 ExecutionLeg.kind 保持一致：0 = V2，1 = V3，2 = BalancerV2。
    enum DexKind {
        V2,
        V3,
        BalancerV2
    }

    // Rust 扫描器输出的单跳交易路径。
    struct SwapLeg {
        DexKind kind;
        address router;
        uint24 fee;
        bytes32 poolId;
        address tokenIn;
        address tokenOut;
    }

    // 只有部署者可以执行套利和救援资产。
    address public immutable owner;

    error NotOwner();
    error EmptyRoute();
    error RouteMismatch();
    error Slippage();
    error TransferFailed();

    constructor() {
        owner = msg.sender;
    }

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    function execute(
        SwapLeg[] calldata legs,
        uint256 amountIn,
        uint256 minAmountOut,
        uint256 deadline
    ) external onlyOwner returns (uint256 amountOut) {
        if (legs.length == 0) revert EmptyRoute();

        // 资金从 owner 钱包拉入合约；运行前需要 owner 对本合约 approve。
        address startToken = legs[0].tokenIn;
        if (!IERC20(startToken).transferFrom(msg.sender, address(this), amountIn)) {
            revert TransferFailed();
        }

        uint256 currentAmount = amountIn;
        address currentToken = startToken;

        for (uint256 i = 0; i < legs.length; i++) {
            SwapLeg calldata leg = legs[i];
            // 每一跳的输入 token 必须等于上一跳输出 token，防止错误路径执行。
            if (leg.tokenIn != currentToken) revert RouteMismatch();

            _approveExact(leg.tokenIn, leg.router, currentAmount);

            if (leg.kind == DexKind.V2) {
                currentAmount = _swapV2(leg, currentAmount, deadline);
            } else if (leg.kind == DexKind.V3) {
                currentAmount = _swapV3(leg, currentAmount, deadline);
            } else if (leg.kind == DexKind.BalancerV2) {
                currentAmount = _swapBalancerV2(leg, currentAmount, deadline);
            } else {
                revert RouteMismatch();
            }

            currentToken = leg.tokenOut;
        }

        // 合规套利必须闭环回到起始 token，不能留下中间资产。
        if (currentToken != startToken) revert RouteMismatch();

        // minAmountOut 由 Rust 侧按“本金 + 最低利润”计算，链上兜底防滑点。
        if (currentAmount < minAmountOut) revert Slippage();

        // 执行完成后把全部起始 token 返还 owner。
        if (!IERC20(startToken).transfer(owner, currentAmount)) revert TransferFailed();

        return currentAmount;
    }

    function rescue(address token, uint256 amount) external onlyOwner {
        if (!IERC20(token).transfer(owner, amount)) revert TransferFailed();
    }

    function _swapV2(SwapLeg calldata leg, uint256 amountIn, uint256 deadline) private returns (uint256) {
        // V2 两 token path：tokenIn -> tokenOut。
        address[] memory path = new address[](2);
        path[0] = leg.tokenIn;
        path[1] = leg.tokenOut;
        uint256[] memory amounts = IUniswapV2Router02(leg.router).swapExactTokensForTokens(
            amountIn,
            0,
            path,
            address(this),
            deadline
        );
        return amounts[amounts.length - 1];
    }

    function _swapV3(SwapLeg calldata leg, uint256 amountIn, uint256 deadline) private returns (uint256) {
        // V3 exactInputSingle：单池单跳 swap。
        return ISwapRouter(leg.router).exactInputSingle(
            ISwapRouter.ExactInputSingleParams({
                tokenIn: leg.tokenIn,
                tokenOut: leg.tokenOut,
                fee: leg.fee,
                recipient: address(this),
                deadline: deadline,
                amountIn: amountIn,
                amountOutMinimum: 0,
                sqrtPriceLimitX96: 0
            })
        );
    }

    function _swapBalancerV2(SwapLeg calldata leg, uint256 amountIn, uint256 deadline) private returns (uint256) {
        // Balancer V2 通过 Vault batchSwap 执行单池单跳。
        uint256 balanceBefore = IERC20(leg.tokenOut).balanceOf(address(this));
        IBalancerV2Vault.BatchSwapStep[] memory swaps = new IBalancerV2Vault.BatchSwapStep[](1);
        swaps[0] = IBalancerV2Vault.BatchSwapStep({
            poolId: leg.poolId,
            assetInIndex: 0,
            assetOutIndex: 1,
            amount: amountIn,
            userData: ""
        });

        address[] memory assets = new address[](2);
        assets[0] = leg.tokenIn;
        assets[1] = leg.tokenOut;

        int256[] memory limits = new int256[](2);
        limits[0] = int256(amountIn);
        limits[1] = 0;

        IBalancerV2Vault(leg.router).batchSwap(
            IBalancerV2Vault.SwapKind.GIVEN_IN,
            swaps,
            assets,
            IBalancerV2Vault.FundManagement({
                sender: address(this),
                fromInternalBalance: false,
                recipient: payable(address(this)),
                toInternalBalance: false
            }),
            limits,
            deadline
        );
        return IERC20(leg.tokenOut).balanceOf(address(this)) - balanceBefore;
    }

    function _approveExact(address token, address spender, uint256 amount) private {
        // 先归零再授权，兼容部分要求先清 allowance 的 ERC-20。
        IERC20(token).approve(spender, 0);
        IERC20(token).approve(spender, amount);
    }
}
