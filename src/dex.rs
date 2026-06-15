use std::sync::Arc;

use async_trait::async_trait;
use ethers::{
    providers::Middleware,
    types::{Address, U256},
};
use eyre::{Result, bail};

use crate::{
    abis::{IUniswapV2Pair, IUniswapV3Quoter},
    config::PoolConfig,
};

#[derive(Clone, Debug, serde::Serialize)]
pub enum DexKind {
    /// Uniswap V2 constant-product 池。
    V2,
    /// Uniswap V3 concentrated-liquidity 池。
    V3,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Quote {
    /// 池子的人类可读名称，来自配置。
    pub pool_name: String,
    /// DEX 类型，便于输出和执行器组装路径。
    pub kind: DexKind,
    /// 输入 token 地址。
    pub token_in: Address,
    /// 输出 token 地址。
    pub token_out: Address,
    #[serde(serialize_with = "crate::scanner::serialize_u256")]
    pub amount_in: U256,
    #[serde(serialize_with = "crate::scanner::serialize_u256")]
    pub amount_out: U256,
}

#[async_trait]
pub trait QuotePool {
    /// 查询给定 token_in 和 amount_in 的精确输入报价。
    async fn quote_exact_input(&self, token_in: Address, amount_in: U256) -> Result<Quote>;
}

#[derive(Clone)]
pub struct PoolClient<M> {
    provider: Arc<M>,
    config: PoolConfig,
}

impl<M> PoolClient<M> {
    pub fn new(provider: Arc<M>, config: PoolConfig) -> Self {
        Self { provider, config }
    }

    pub fn config(&self) -> &PoolConfig {
        &self.config
    }
}

#[async_trait]
impl<M> QuotePool for PoolClient<M>
where
    M: Middleware + 'static,
{
    async fn quote_exact_input(&self, token_in: Address, amount_in: U256) -> Result<Quote> {
        match &self.config {
            PoolConfig::V2 {
                name,
                pair,
                token0,
                token1,
                fee_bps,
                ..
            } => {
                // V2 报价不依赖路由器，直接读取 reserves 后套用官方 x*y=k 公式。
                let (token_out, reserve_in, reserve_out) =
                    v2_reserves(self.provider.clone(), *pair, *token0, *token1, token_in).await?;
                let amount_out = v2_amount_out(amount_in, reserve_in, reserve_out, *fee_bps)?;
                Ok(Quote {
                    pool_name: name.clone(),
                    kind: DexKind::V2,
                    token_in,
                    token_out,
                    amount_in,
                    amount_out,
                })
            }
            PoolConfig::V3 {
                name,
                quoter,
                token0,
                token1,
                fee,
                ..
            } => {
                let token_out = other_token(*token0, *token1, token_in)?;
                let quoter = IUniswapV3Quoter::new(*quoter, self.provider.clone());
                // Quoter 的 call 是 eth_call，本地模拟，不会产生链上状态变更。
                let amount_out = quoter
                    .quote_exact_input_single(token_in, token_out, *fee, amount_in, U256::zero())
                    .call()
                    .await?;
                Ok(Quote {
                    pool_name: name.clone(),
                    kind: DexKind::V3,
                    token_in,
                    token_out,
                    amount_in,
                    amount_out,
                })
            }
        }
    }
}

async fn v2_reserves(
    provider: Arc<impl Middleware + 'static>,
    pair: Address,
    token0: Address,
    token1: Address,
    token_in: Address,
) -> Result<(Address, U256, U256)> {
    let pair = IUniswapV2Pair::new(pair, provider);
    let (reserve0, reserve1, _) = pair.get_reserves().call().await?;
    if token_in == token0 {
        Ok((token1, reserve0.into(), reserve1.into()))
    } else if token_in == token1 {
        Ok((token0, reserve1.into(), reserve0.into()))
    } else {
        bail!("token {token_in:?} is not in configured V2 pair");
    }
}

pub fn other_token(token0: Address, token1: Address, token_in: Address) -> Result<Address> {
    if token_in == token0 {
        Ok(token1)
    } else if token_in == token1 {
        Ok(token0)
    } else {
        bail!("token {token_in:?} is not in pool {token0:?}/{token1:?}");
    }
}

pub fn v2_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u32,
) -> Result<U256> {
    if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
        return Ok(U256::zero());
    }
    if fee_bps >= 10_000 {
        bail!("fee_bps must be less than 10000");
    }

    // Uniswap V2 公式：
    // amountOut = amountInWithFee * reserveOut / (reserveIn * 10000 + amountInWithFee)
    let fee_denominator = U256::from(10_000_u64);
    let fee_multiplier = U256::from(10_000_u64 - fee_bps as u64);
    let amount_in_with_fee = amount_in * fee_multiplier;
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in * fee_denominator + amount_in_with_fee;
    Ok(numerator / denominator)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_amount_out_matches_uniswap_formula() {
        let amount = U256::from(1_000_u64);
        let out = v2_amount_out(amount, U256::from(10_000_u64), U256::from(20_000_u64), 30)
            .expect("quote");
        assert_eq!(out, U256::from(1_813_u64));
    }

    #[test]
    fn zero_liquidity_quotes_zero() {
        let out =
            v2_amount_out(U256::from(1_u64), U256::zero(), U256::from(10_u64), 30).expect("quote");
        assert_eq!(out, U256::zero());
    }
}
