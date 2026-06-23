use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use ethers::{
    providers::Middleware,
    types::{Address, H256, U256},
    utils::parse_units,
};
use eyre::Result;
use serde::{Serialize, Serializer};
use tracing::{debug, info, warn};

use crate::{
    config::{BotConfig, PoolConfig, parse_u256_dec},
    dex::{PoolClient, QuotePool},
};

#[derive(Clone)]
pub struct Scanner<M> {
    /// 共享 RPC provider。
    provider: Arc<M>,
    /// 完整机器人配置。
    cfg: BotConfig,
}

#[derive(Clone, Debug, Serialize)]
pub struct Opportunity {
    /// 第一跳使用的池名称。
    pub first_pool: String,
    /// 第二跳使用的池名称。
    pub second_pool: String,
    /// 套利起始 token，也是闭环结束 token。
    pub token_start: Address,
    /// 第一跳换出的中间 token。
    pub token_mid: Address,
    #[serde(serialize_with = "serialize_u256")]
    pub amount_in: U256,
    #[serde(serialize_with = "serialize_u256")]
    pub amount_after_first: U256,
    #[serde(serialize_with = "serialize_u256")]
    pub amount_out: U256,
    /// 未扣 gas 的毛利润，单位为 token_start 的最小单位。
    #[serde(serialize_with = "serialize_i256")]
    pub gross_profit_wei: i128,
    /// 估算 gas 后的净利润；只有 token_start 是 native_wrapped_token 时才会扣 gas。
    #[serde(serialize_with = "serialize_i256")]
    pub estimated_net_profit_wei: i128,
    /// 利润 bps；100 bps = 1%。
    pub profit_bps: i128,
    /// 是否已经按 gas_limit 和 max_gas_price_gwei 扣除 gas。
    pub gas_adjusted: bool,
    /// 发现机会的 Unix 时间戳。
    pub unix_ts: u64,
    /// 给执行合约使用的两跳路径。
    pub legs: Vec<ExecutionLeg>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionLeg {
    /// 0 = V2，1 = V3，2 = BalancerV2；需要和 Solidity 枚举保持一致。
    pub kind: u8,
    /// 对应 swap router 地址。
    pub router: Address,
    /// V3 fee tier；V2 固定填 0。
    pub fee: u32,
    /// Balancer V2 poolId；非 Balancer 路线填 0。
    pub pool_id: H256,
    pub token_in: Address,
    pub token_out: Address,
}

impl<M> Scanner<M>
where
    M: Middleware + 'static,
{
    pub fn new(provider: Arc<M>, cfg: BotConfig) -> Self {
        Self { provider, cfg }
    }

    pub fn provider(&self) -> Arc<M> {
        self.provider.clone()
    }

    pub async fn scan(&self) -> Result<Vec<Opportunity>> {
        let mut opportunities = Vec::new();
        // 每次扫描都从配置构建轻量 PoolClient；provider 内部是 Arc 共享。
        let pools: Vec<_> = self
            .cfg
            .pools
            .iter()
            .cloned()
            .map(|pool| PoolClient::new(self.provider.clone(), pool))
            .collect();

        for trade in &self.cfg.trade_sizes {
            let amount_in = parse_u256_dec(&trade.amount_wei)?;
            for (i, first) in pools.iter().enumerate() {
                // 第一跳必须包含起始 token。
                if !first.config().contains_token(trade.token) {
                    self.log_quote_skip(
                        "first_pool_missing_start_token",
                        first.config(),
                        None,
                        trade.token,
                        amount_in,
                    );
                    continue;
                }

                for (j, second) in pools.iter().enumerate() {
                    if i == j {
                        continue;
                    }
                    // 当前版本只做同一 token pair 的两池价差套利。
                    if !same_token_pair(first.config(), second.config()) {
                        self.log_quote_skip(
                            "pool_pair_mismatch",
                            first.config(),
                            Some(second.config()),
                            trade.token,
                            amount_in,
                        );
                        continue;
                    }

                    match self.try_route(first, second, trade.token, amount_in).await {
                        Ok(Some(opportunity)) => opportunities.push(opportunity),
                        Ok(None) => {}
                        Err(err) => {
                            debug!(
                                ?err,
                                first = first.config().name(),
                                second = second.config().name(),
                                "route skipped"
                            );
                            if self.cfg.debug_quotes {
                                info!(
                                    reason = "quote_error",
                                    error = %err,
                                    first_pool = first.config().name(),
                                    second_pool = second.config().name(),
                                    token_start = ?trade.token,
                                    amount_in = %amount_in,
                                    "debug quote route skipped"
                                );
                            }
                        }
                    }
                }
            }
        }

        opportunities.sort_by(|a, b| b.estimated_net_profit_wei.cmp(&a.estimated_net_profit_wei));
        Ok(opportunities)
    }

    async fn try_route(
        &self,
        first: &PoolClient<M>,
        second: &PoolClient<M>,
        token_start: Address,
        amount_in: U256,
    ) -> Result<Option<Opportunity>> {
        // 第一跳：token_start -> token_mid。
        let q1 = first.quote_exact_input(token_start, amount_in).await?;
        if q1.amount_out.is_zero() {
            self.log_zero_first_quote(first.config(), token_start, amount_in);
            return Ok(None);
        }
        // 第二跳：token_mid -> token_start，必须形成闭环。
        let q2 = second
            .quote_exact_input(q1.token_out, q1.amount_out)
            .await?;
        if q2.token_out != token_start || q2.amount_out <= amount_in {
            self.log_unprofitable_or_not_closed(
                first.config(),
                second.config(),
                token_start,
                amount_in,
                q1.amount_out,
                q1.token_out,
                q2.amount_out,
                q2.token_out,
            );
            return Ok(None);
        }

        let gross_profit = u256_delta(q2.amount_out, amount_in);
        // 如果起始 token 是 WETH 这类 wrapped native token，利润里扣掉估算 gas。
        let (estimated_net_profit, gas_adjusted) = self.apply_gas_cost(token_start, gross_profit);
        let profit_bps = estimated_net_profit * 10_000 / u256_to_i128(amount_in);

        if profit_bps < self.cfg.min_profit_bps as i128 {
            self.log_below_threshold(
                first.config(),
                second.config(),
                token_start,
                amount_in,
                q1.amount_out,
                q1.token_out,
                q2.amount_out,
                gross_profit,
                estimated_net_profit,
                profit_bps,
                gas_adjusted,
            );
            return Ok(None);
        }

        self.log_profitable(
            first.config(),
            second.config(),
            token_start,
            amount_in,
            q1.amount_out,
            q1.token_out,
            q2.amount_out,
            gross_profit,
            estimated_net_profit,
            profit_bps,
            gas_adjusted,
        );

        if !gas_adjusted {
            warn!(
                token = ?token_start,
                "profit is gross-only because start token is not native_wrapped_token"
            );
        }

        Ok(Some(Opportunity {
            first_pool: first.config().name().to_string(),
            second_pool: second.config().name().to_string(),
            token_start,
            token_mid: q1.token_out,
            amount_in,
            amount_after_first: q1.amount_out,
            amount_out: q2.amount_out,
            gross_profit_wei: gross_profit,
            estimated_net_profit_wei: estimated_net_profit,
            profit_bps,
            gas_adjusted,
            unix_ts: now_unix_ts(),
            legs: vec![
                leg_from_pool(first.config(), token_start, q1.token_out),
                leg_from_pool(second.config(), q1.token_out, token_start),
            ],
        }))
    }

    fn apply_gas_cost(&self, token_start: Address, gross_profit: i128) -> (i128, bool) {
        let Some(wrapped) = self.cfg.native_wrapped_token else {
            return (gross_profit, false);
        };
        if wrapped != token_start {
            return (gross_profit, false);
        }
        let gas_price: U256 = parse_units(self.cfg.max_gas_price_gwei, "gwei")
            .expect("static unit")
            .into();
        let gas_cost = U256::from(self.cfg.gas_limit) * gas_price;
        (gross_profit - u256_to_i128(gas_cost), true)
    }

    fn log_quote_skip(
        &self,
        reason: &'static str,
        first: &PoolConfig,
        second: Option<&PoolConfig>,
        token_start: Address,
        amount_in: U256,
    ) {
        if !self.cfg.debug_quotes {
            return;
        }
        info!(
            reason,
            first_pool = first.name(),
            second_pool = second.map(PoolConfig::name).unwrap_or("-"),
            token_start = ?token_start,
            amount_in = %amount_in,
            "debug quote route skipped"
        );
    }

    fn log_zero_first_quote(&self, first: &PoolConfig, token_start: Address, amount_in: U256) {
        if !self.cfg.debug_quotes {
            return;
        }
        info!(
            reason = "first_quote_zero",
            first_pool = first.name(),
            token_start = ?token_start,
            amount_in = %amount_in,
            "debug quote route skipped"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn log_unprofitable_or_not_closed(
        &self,
        first: &PoolConfig,
        second: &PoolConfig,
        token_start: Address,
        amount_in: U256,
        amount_after_first: U256,
        token_mid: Address,
        amount_out: U256,
        token_out: Address,
    ) {
        if !self.cfg.debug_quotes {
            return;
        }
        let reason = if token_out != token_start {
            "route_not_closed"
        } else {
            "not_gross_profitable"
        };
        let gross_profit = u256_delta(amount_out, amount_in);
        info!(
            reason,
            first_pool = first.name(),
            second_pool = second.name(),
            token_start = ?token_start,
            token_mid = ?token_mid,
            token_out = ?token_out,
            amount_in = %amount_in,
            amount_after_first = %amount_after_first,
            amount_out = %amount_out,
            gross_profit_wei = %gross_profit,
            "debug quote route result"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn log_below_threshold(
        &self,
        first: &PoolConfig,
        second: &PoolConfig,
        token_start: Address,
        amount_in: U256,
        amount_after_first: U256,
        token_mid: Address,
        amount_out: U256,
        gross_profit: i128,
        estimated_net_profit: i128,
        profit_bps: i128,
        gas_adjusted: bool,
    ) {
        if !self.cfg.debug_quotes {
            return;
        }
        info!(
            reason = "below_min_profit_bps",
            first_pool = first.name(),
            second_pool = second.name(),
            token_start = ?token_start,
            token_mid = ?token_mid,
            amount_in = %amount_in,
            amount_after_first = %amount_after_first,
            amount_out = %amount_out,
            gross_profit_wei = %gross_profit,
            estimated_net_profit_wei = %estimated_net_profit,
            profit_bps,
            min_profit_bps = self.cfg.min_profit_bps,
            gas_adjusted,
            "debug quote route result"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn log_profitable(
        &self,
        first: &PoolConfig,
        second: &PoolConfig,
        token_start: Address,
        amount_in: U256,
        amount_after_first: U256,
        token_mid: Address,
        amount_out: U256,
        gross_profit: i128,
        estimated_net_profit: i128,
        profit_bps: i128,
        gas_adjusted: bool,
    ) {
        if !self.cfg.debug_quotes {
            return;
        }
        info!(
            reason = "profitable",
            first_pool = first.name(),
            second_pool = second.name(),
            token_start = ?token_start,
            token_mid = ?token_mid,
            amount_in = %amount_in,
            amount_after_first = %amount_after_first,
            amount_out = %amount_out,
            gross_profit_wei = %gross_profit,
            estimated_net_profit_wei = %estimated_net_profit,
            profit_bps,
            min_profit_bps = self.cfg.min_profit_bps,
            gas_adjusted,
            "debug quote route result"
        );
    }
}

fn same_token_pair(a: &PoolConfig, b: &PoolConfig) -> bool {
    (a.token0() == b.token0() && a.token1() == b.token1())
        || (a.token0() == b.token1() && a.token1() == b.token0())
}

fn leg_from_pool(pool: &PoolConfig, token_in: Address, token_out: Address) -> ExecutionLeg {
    // 这里的 kind 必须和 contracts/ArbExecutor.sol 里的 DexKind 枚举顺序一致。
    let (kind, fee, pool_id) = match pool {
        PoolConfig::V2 { .. } => (0, 0, H256::zero()),
        PoolConfig::V3 { fee, .. } => (1, *fee, H256::zero()),
        PoolConfig::BalancerV2 { pool_id, .. } => (2, 0, *pool_id),
    };
    ExecutionLeg {
        kind,
        router: pool.router(),
        fee,
        pool_id,
        token_in,
        token_out,
    }
}

fn now_unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time before epoch")
        .as_secs()
}

fn u256_delta(a: U256, b: U256) -> i128 {
    if a >= b {
        u256_to_i128(a - b)
    } else {
        -u256_to_i128(b - a)
    }
}

fn u256_to_i128(value: U256) -> i128 {
    // 输出只用于日志/JSON；极端大数饱和到 i128::MAX，避免转换 panic。
    let max = U256::from(i128::MAX as u128);
    if value > max {
        i128::MAX
    } else {
        value.as_u128() as i128
    }
}

pub fn serialize_u256<S>(value: &U256, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

pub fn serialize_i256<S>(value: &i128, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_same_token_pair_regardless_order() {
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let pair_a = PoolConfig::V2 {
            name: "a".into(),
            pair: Address::from_low_u64_be(3),
            router: Address::from_low_u64_be(4),
            token0: a,
            token1: b,
            fee_bps: 30,
        };
        let pair_b = PoolConfig::V3 {
            name: "b".into(),
            quoter: Address::from_low_u64_be(5),
            router: Address::from_low_u64_be(6),
            token0: b,
            token1: a,
            fee: 500,
        };
        assert!(same_token_pair(&pair_a, &pair_b));
    }
}
