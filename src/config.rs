use std::{env, fs, path::Path};

use ethers::types::Address;
use eyre::{Result, bail, eyre};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BotConfig {
    /// 以太坊 RPC 地址，建议放在 .env 的 RPC_URL。
    pub rpc_url: String,
    /// WSS RPC 地址；watch-blocks 模式优先使用它订阅新区块。
    pub ws_rpc_url: Option<String>,
    /// 持续扫描模式的轮询间隔，单位秒。
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// dry_run 只输出机会；live 会调用自有执行合约。
    #[serde(default)]
    pub run_mode: RunMode,
    /// 链原生资产的 wrapped token 地址，例如主网 WETH；用于估算 gas 后净利润。
    pub native_wrapped_token: Option<Address>,
    /// 最小利润阈值，单位 bps；20 表示 0.20%。
    #[serde(default = "default_min_profit_bps")]
    pub min_profit_bps: u32,
    /// 预估执行交易的 gas limit，用来扣减净利润。
    #[serde(default = "default_gas_limit")]
    pub gas_limit: u64,
    /// 最大 gas price，单位 gwei；用于保守估算 gas 成本。
    #[serde(default = "default_max_gas_price_gwei")]
    pub max_gas_price_gwei: u64,
    /// 调试报价开关；开启后打印每条路线报价和过滤原因。
    #[serde(default)]
    pub debug_quotes: bool,
    /// 每次扫描尝试的起始 token 和输入金额列表。
    #[serde(default)]
    pub trade_sizes: Vec<TradeSize>,
    /// 参与比较的 Uniswap V2/V3 池配置。
    #[serde(default)]
    pub pools: Vec<PoolConfig>,
    /// live 模式需要的执行合约和签名私钥配置。
    pub executor: Option<ExecutorConfig>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// 安全默认值：只扫描、只打印，不发送交易。
    #[default]
    DryRun,
    /// 实盘模式：发现机会后调用配置的 executor 合约。
    Live,
}

impl RunMode {
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TradeSize {
    /// 起始 token 地址；两跳套利最终也必须回到这个 token。
    pub token: Address,
    /// 输入金额，使用 wei 的十进制字符串，避免浮点精度问题。
    pub amount_wei: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutorConfig {
    /// 已部署的 ArbExecutor 合约地址。
    pub address: Address,
    /// 交易签名私钥；必须放在 .env，不要提交真实值。
    pub private_key: String,
    /// 执行交易 deadline，单位秒。
    #[serde(default = "default_deadline_secs")]
    pub deadline_secs: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PoolConfig {
    /// Uniswap V2 风格池：用 pair reserves 在本地计算报价。
    V2 {
        name: String,
        pair: Address,
        router: Address,
        token0: Address,
        token1: Address,
        #[serde(default = "default_v2_fee_bps")]
        fee_bps: u32,
    },
    /// Uniswap V3 池：用 Quoter 合约 eth_call 获取报价。
    V3 {
        name: String,
        quoter: Address,
        router: Address,
        token0: Address,
        token1: Address,
        fee: u32,
    },
}

impl PoolConfig {
    pub fn name(&self) -> &str {
        match self {
            Self::V2 { name, .. } | Self::V3 { name, .. } => name,
        }
    }

    pub fn router(&self) -> Address {
        match self {
            Self::V2 { router, .. } | Self::V3 { router, .. } => *router,
        }
    }

    pub fn token0(&self) -> Address {
        match self {
            Self::V2 { token0, .. } | Self::V3 { token0, .. } => *token0,
        }
    }

    pub fn token1(&self) -> Address {
        match self {
            Self::V2 { token1, .. } | Self::V3 { token1, .. } => *token1,
        }
    }

    pub fn contains_token(&self, token: Address) -> bool {
        self.token0() == token || self.token1() == token
    }
}

impl BotConfig {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        // 先替换 ${VAR}，再把 YAML 解析成强类型配置。
        let expanded = expand_env_vars(&raw)?;
        Ok(serde_yaml::from_str(&expanded)?)
    }

    pub fn validate(&self) -> Result<()> {
        if self.rpc_url.trim().is_empty() {
            bail!("rpc_url cannot be empty");
        }
        if self.trade_sizes.is_empty() {
            bail!("configure at least one trade_sizes entry");
        }
        if self.pools.len() < 2 {
            bail!("configure at least two pools");
        }
        if self.run_mode.is_live() && self.executor.is_none() {
            bail!("live mode requires executor.address and executor.private_key");
        }
        for trade in &self.trade_sizes {
            parse_u256_dec(&trade.amount_wei)?;
        }
        for pool in &self.pools {
            if pool.token0() == pool.token1() {
                bail!("pool {} has identical token0/token1", pool.name());
            }
        }
        Ok(())
    }

    pub fn ws_rpc_url(&self) -> Option<&str> {
        self.ws_rpc_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

fn expand_env_vars(raw: &str) -> Result<String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            bail!("unterminated environment variable placeholder in config");
        };
        let name = &after_start[..end];
        if name.is_empty() {
            bail!("empty environment variable placeholder in config");
        }
        let value = env::var(name).map_err(|_| eyre!("missing environment variable {name}"))?;
        out.push_str(&value);
        rest = &after_start[end + 1..];
    }

    out.push_str(rest);
    Ok(out)
}

pub fn parse_u256_dec(value: &str) -> Result<ethers::types::U256> {
    ethers::types::U256::from_dec_str(value)
        .map_err(|err| eyre!("invalid decimal U256 {value}: {err}"))
}

fn default_poll_interval_secs() -> u64 {
    12
}

fn default_min_profit_bps() -> u32 {
    20
}

fn default_gas_limit() -> u64 {
    350_000
}

fn default_max_gas_price_gwei() -> u64 {
    35
}

fn default_v2_fee_bps() -> u32 {
    30
}

fn default_deadline_secs() -> u64 {
    60
}
