use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use ethers::{
    abi::Abi,
    contract::Contract,
    middleware::SignerMiddleware,
    providers::Middleware,
    signers::{LocalWallet, Signer},
    types::{Address, H256, U256},
};
use eyre::{Result, bail};
use tracing::info;

use crate::{config::BotConfig, scanner::Opportunity};

// Rust 侧只需要调用执行合约的 execute 方法，因此内嵌最小 ABI。
const EXECUTOR_ABI: &str = r#"[
  {
    "inputs": [
      {
        "components": [
          {"internalType":"uint8","name":"kind","type":"uint8"},
          {"internalType":"address","name":"router","type":"address"},
          {"internalType":"uint24","name":"fee","type":"uint24"},
          {"internalType":"bytes32","name":"poolId","type":"bytes32"},
          {"internalType":"address","name":"tokenIn","type":"address"},
          {"internalType":"address","name":"tokenOut","type":"address"}
        ],
        "internalType":"struct SwapLeg[]",
        "name":"legs",
        "type":"tuple[]"
      },
      {"internalType":"uint256","name":"amountIn","type":"uint256"},
      {"internalType":"uint256","name":"minAmountOut","type":"uint256"},
      {"internalType":"uint256","name":"deadline","type":"uint256"}
    ],
    "name":"execute",
    "outputs":[{"internalType":"uint256","name":"amountOut","type":"uint256"}],
    "stateMutability":"nonpayable",
    "type":"function"
  }
]"#;

pub async fn execute(
    cfg: &BotConfig,
    provider: Arc<impl Middleware + Clone + 'static>,
    opportunity: &Opportunity,
) -> Result<H256> {
    let Some(executor) = &cfg.executor else {
        bail!("live execution requested without executor config");
    };

    // 用链 ID 绑定签名，防止交易被错误重放到其他链。
    let chain_id = provider.get_chainid().await?.as_u64();
    let wallet: LocalWallet = executor
        .private_key
        .parse::<LocalWallet>()?
        .with_chain_id(chain_id);
    let client = Arc::new(SignerMiddleware::new((*provider).clone(), wallet));
    let abi: Abi = serde_json::from_str(EXECUTOR_ABI)?;
    let contract = Contract::new(executor.address, abi, client);

    // minAmountOut = 本金 + 最低利润，用合约层防止成交后利润不足。
    let min_amount_out = min_amount_out(opportunity.amount_in, cfg.min_profit_bps);
    let deadline = U256::from(now_unix_ts() + executor.deadline_secs);
    let legs: Vec<(u8, Address, u32, ethers::types::H256, Address, Address)> = opportunity
        .legs
        .iter()
        .map(|leg| {
            (
                leg.kind,
                leg.router,
                leg.fee,
                leg.pool_id,
                leg.token_in,
                leg.token_out,
            )
        })
        .collect();

    let call = contract.method::<_, U256>(
        "execute",
        (legs, opportunity.amount_in, min_amount_out, deadline),
    )?;
    let pending = call.send().await?;
    let tx_hash = pending.tx_hash();
    info!(?tx_hash, "submitted arbitrage execution transaction");
    Ok(tx_hash)
}

fn min_amount_out(amount_in: U256, min_profit_bps: u32) -> U256 {
    amount_in * U256::from(10_000_u64 + min_profit_bps as u64) / U256::from(10_000_u64)
}

fn now_unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time before epoch")
        .as_secs()
}
