mod abis;
mod config;
mod dex;
mod executor;
mod scanner;

use std::sync::Arc;

use clap::{Parser, Subcommand};
use config::BotConfig;
use ethers::providers::{Http, Middleware, Provider, Ws};
use eyre::{Context, Result, bail};
use futures_util::StreamExt;
use scanner::Scanner;
use tokio::time::{self, Duration};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    /// 配置文件路径；配置文件里可以使用 ${VAR} 引用 .env 中的变量。
    #[arg(short, long, default_value = "config.example.yaml")]
    config: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 扫描一次，并把发现的套利机会按 JSON 行输出。
    Once,
    /// 按配置的间隔持续扫描。
    Watch,
    /// 通过 WSS 订阅新区块，每个新区块产生后立刻扫描。
    WatchBlocks,
}

#[tokio::main]
async fn main() -> Result<()> {
    // 自动读取当前目录下的 .env，敏感参数不要写死在代码或提交到仓库。
    dotenvy::dotenv().ok();

    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();
    let cfg = BotConfig::from_path(&cli.config)
        .wrap_err_with(|| format!("failed to load config {}", cli.config))?;
    cfg.validate()?;

    match cli.command {
        Command::Once => {
            let provider = connect_http_or_ws(&cfg.rpc_url).await?;
            run_once_with_provider(provider, cfg).await?;
        }
        Command::Watch => {
            let provider = connect_http_or_ws(&cfg.rpc_url).await?;
            run_watch_with_provider(provider, cfg).await?;
        }
        Command::WatchBlocks => {
            let provider = connect_ws_for_blocks(&cfg).await?;
            let scanner = Scanner::new(provider, cfg.clone());
            watch_blocks(scanner, cfg).await?;
        }
    }

    Ok(())
}

enum RpcProvider {
    Http(Arc<Provider<Http>>),
    Ws(Arc<Provider<Ws>>),
}

async fn connect_http_or_ws(rpc_url: &str) -> Result<RpcProvider> {
    if rpc_url.starts_with("ws://") || rpc_url.starts_with("wss://") {
        let ws = Ws::connect(rpc_url).await?;
        Ok(RpcProvider::Ws(Arc::new(Provider::new(ws))))
    } else {
        Ok(RpcProvider::Http(Arc::new(Provider::<Http>::try_from(
            rpc_url,
        )?)))
    }
}

async fn connect_ws_for_blocks(cfg: &BotConfig) -> Result<Arc<Provider<Ws>>> {
    let ws_url = cfg.ws_rpc_url().unwrap_or(&cfg.rpc_url);
    if !ws_url.starts_with("ws://") && !ws_url.starts_with("wss://") {
        bail!("watch-blocks requires ws_rpc_url or rpc_url to start with ws:// or wss://");
    }

    let ws = Ws::connect(ws_url).await?;
    Ok(Arc::new(Provider::new(ws)))
}

async fn run_once_with_provider(provider: RpcProvider, cfg: BotConfig) -> Result<()> {
    match provider {
        RpcProvider::Http(provider) => {
            let scanner = Scanner::new(provider, cfg.clone());
            scan_once(&scanner, &cfg).await
        }
        RpcProvider::Ws(provider) => {
            let scanner = Scanner::new(provider, cfg.clone());
            scan_once(&scanner, &cfg).await
        }
    }
}

async fn run_watch_with_provider(provider: RpcProvider, cfg: BotConfig) -> Result<()> {
    match provider {
        RpcProvider::Http(provider) => {
            let scanner = Scanner::new(provider, cfg.clone());
            watch(scanner, cfg).await
        }
        RpcProvider::Ws(provider) => {
            let scanner = Scanner::new(provider, cfg.clone());
            watch(scanner, cfg).await
        }
    }
}

async fn watch<M>(scanner: Scanner<M>, cfg: BotConfig) -> Result<()>
where
    M: Middleware + Clone + 'static,
{
    let mut interval = time::interval(Duration::from_secs(cfg.poll_interval_secs));
    info!(
        poll_interval_secs = cfg.poll_interval_secs,
        mode = ?cfg.run_mode,
        "starting compliant arbitrage scanner"
    );

    loop {
        tokio::select! {
            // 支持 Ctrl-C 优雅退出，避免循环扫描进程被强杀。
            _ = tokio::signal::ctrl_c() => {
                warn!("received ctrl-c, shutting down");
                break;
            }
            _ = interval.tick() => {
                if let Err(err) = scan_once(&scanner, &cfg).await {
                    error!(?err, "scan failed");
                }
            }
        }
    }

    Ok(())
}

async fn watch_blocks(scanner: Scanner<Provider<Ws>>, cfg: BotConfig) -> Result<()> {
    let provider = scanner.provider();
    let mut blocks = provider.subscribe_blocks().await?;

    info!(
        mode = ?cfg.run_mode,
        "starting WSS block subscription scanner"
    );

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                warn!("received ctrl-c, shutting down");
                break;
            }
            maybe_block = blocks.next() => {
                let Some(block) = maybe_block else {
                    warn!("block subscription ended");
                    break;
                };
                info!(
                    block_number = ?block.number,
                    block_hash = ?block.hash,
                    "new block received, scanning confirmed pool state"
                );
                if let Err(err) = scan_once(&scanner, &cfg).await {
                    error!(?err, "scan failed");
                }
            }
        }
    }

    Ok(())
}

async fn scan_once<M>(scanner: &Scanner<M>, cfg: &BotConfig) -> Result<()>
where
    M: Middleware + Clone + 'static,
{
    let opportunities = scanner.scan().await?;
    if opportunities.is_empty() {
        info!("no profitable opportunities found");
        return Ok(());
    }

    for opportunity in opportunities {
        warn!(
            first_pool = opportunity.first_pool,
            second_pool = opportunity.second_pool,
            token_start = ?opportunity.token_start,
            token_mid = ?opportunity.token_mid,
            amount_in = %opportunity.amount_in,
            amount_after_first = %opportunity.amount_after_first,
            amount_out = %opportunity.amount_out,
            gross_profit_wei = %opportunity.gross_profit_wei,
            estimated_net_profit_wei = %opportunity.estimated_net_profit_wei,
            profit_bps = opportunity.profit_bps,
            gas_adjusted = opportunity.gas_adjusted,
            "🚀💰 ARBITRAGE OPPORTUNITY FOUND 💰🚀"
        );
        println!("{}", serde_json::to_string(&opportunity)?);
        // live 模式只调用你自己的执行合约，不监听 pending transaction，也不做抢跑/夹子。
        if cfg.run_mode.is_live() {
            executor::execute(cfg, scanner.provider(), &opportunity).await?;
        }
    }

    Ok(())
}
