# Uniswap Arbitrage Bot

Rust bot for compliant Uniswap V2/V3 arbitrage scanning. It only uses confirmed on-chain state and `eth_call` quotes. It does not inspect the mempool, front-run users, back-run users, or build sandwich bundles.

## What It Does

- Scans configured Uniswap V2/V3 pools for two-leg closed-loop arbitrage.
- Quotes V2 pools from pair reserves.
- Quotes V3 pools through the Uniswap V3 Quoter contract.
- Applies a configurable minimum profit threshold.
- Deducts estimated gas when the start token is `native_wrapped_token`.
- Runs in `dry_run` by default and prints opportunities as JSON.
- Supports optional `live` execution through your own deployed executor contract.
- Example config scans WETH pairs for LINK, UNI, AAVE, and WBTC.
- Sends optional Telegram alerts when an arbitrage opportunity is found.

## Quick Start

```bash
cp .env.example .env
cp config.example.yaml config.yaml
# edit .env, then adjust pools in config.yaml if needed
cargo run -- --config config.yaml once
```

按固定间隔持续扫描：

```bash
RUST_LOG=info cargo run -- --config config.yaml watch
```

通过 WSS 订阅新区块，每个确认区块出来后立刻扫描：

```bash
cargo run -- --config config.yaml watch-blocks
```

## Configuration Notes

The bot loads `.env` automatically on startup. `config.yaml` supports `${VAR}` placeholders, so keep secrets like `RPC_URL` and `PRIVATE_KEY` in `.env` instead of committing them.

`watch-blocks` 需要配置 `WS_RPC_URL`，或者把 `RPC_URL` 设置成 `wss://` 开头。它只订阅新区块，不订阅 pending transaction。

Telegram alerts are disabled by default. To enable them, set:

```env
TELEGRAM_ENABLED=true
TELEGRAM_BOT_TOKEN=123456:your_bot_token
TELEGRAM_CHAT_ID=123456789
```

To test real Telegram delivery:

```bash
cargo test sends_real_telegram_message_when_env_is_configured -- --ignored --nocapture
```

`trade_sizes` should be conservative. A profitable quote can disappear before your transaction lands, and larger trades create more price impact.

`min_profit_bps` is applied after gas only when `token_start == native_wrapped_token`. For non-native start tokens, the bot reports gross token profit because gas is paid in ETH, not the ERC-20 token.

V3 uses the legacy `quoteExactInputSingle(address,address,uint24,uint256,uint160)` Quoter ABI. On Ethereum mainnet that is commonly `0xb27308f9F90D607463bb33eA1BeBb41C27CE5AB6`.

## Live Mode

Live mode requires:

1. Deploying an execution contract compatible with `contracts/ArbExecutor.sol`.
2. Funding or approving the executor for the input token.
3. Setting:

```yaml
run_mode: live
executor:
  address: "0xYourExecutorContract"
  private_key: "${PRIVATE_KEY}"
```

The Rust bot calls `execute(legs, amountIn, minAmountOut, deadline)` on the executor. It does not submit private bundles and does not react to pending transactions.

## Safety Checklist

- Test on a fork before mainnet.
- Keep `run_mode: dry_run` until quotes and route construction are verified.
- Use small trade sizes first.
- Keep a strict `min_profit_bps`.
- Use a dedicated wallet with limited funds.
- Consider private transaction submission for your own transaction privacy, not for user-targeting.

## Development

```bash
cargo fmt
cargo test
```
