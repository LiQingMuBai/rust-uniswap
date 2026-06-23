use reqwest::Client;
use serde::Serialize;
use tracing::{debug, error};

use crate::{config::BotConfig, scanner::Opportunity};

#[derive(Debug, Serialize)]
struct TelegramSendMessage<'a> {
    chat_id: &'a str,
    text: String,
    parse_mode: &'static str,
    disable_web_page_preview: bool,
}

/// 发现套利机会时异步通知 Telegram。
///
/// 这里故意不 await 网络请求，避免 Telegram API 慢或失败时阻塞扫描主流程。
pub fn notify_opportunity(cfg: &BotConfig, opportunity: &Opportunity) {
    let Some(telegram) = cfg.telegram.as_ref() else {
        return;
    };
    if !telegram.enabled {
        return;
    }

    let bot_token = telegram.bot_token.clone();
    let chat_id = telegram.chat_id.clone();
    let text = format_opportunity(opportunity);

    tokio::spawn(async move {
        if let Err(err) = send_telegram_message(&bot_token, &chat_id, text).await {
            error!(?err, "failed to send telegram opportunity notification");
        }
    });
}

async fn send_telegram_message(bot_token: &str, chat_id: &str, text: String) -> eyre::Result<()> {
    let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");
    let payload = TelegramSendMessage {
        chat_id,
        text,
        parse_mode: "HTML",
        disable_web_page_preview: true,
    };

    let response = Client::new().post(url).json(&payload).send().await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        eyre::bail!("telegram sendMessage failed: status={status}, body={body}");
    }

    debug!(status = %status, "telegram opportunity notification sent");
    Ok(())
}

fn format_opportunity(opportunity: &Opportunity) -> String {
    format!(
        "🚀💰 <b>发现套利机会</b> 💰🚀\n\
         \n\
         <b>路径</b>: {} → {}\n\
         <b>Token</b>: {:?} → {:?} → {:?}\n\
         <b>输入</b>: {}\n\
         <b>第一跳输出</b>: {}\n\
         <b>最终输出</b>: {}\n\
         <b>毛利润 wei</b>: {}\n\
         <b>预估净利润 wei</b>: {}\n\
         <b>利润 bps</b>: {}\n\
         <b>已扣 gas</b>: {}",
        html_escape(&opportunity.first_pool),
        html_escape(&opportunity.second_pool),
        opportunity.token_start,
        opportunity.token_mid,
        opportunity.token_start,
        opportunity.amount_in,
        opportunity.amount_after_first,
        opportunity.amount_out,
        opportunity.gross_profit_wei,
        opportunity.estimated_net_profit_wei,
        opportunity.profit_bps,
        opportunity.gas_adjusted,
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use std::env;

    use ethers::types::{Address, H256, U256};

    use super::*;
    use crate::scanner::{ExecutionLeg, Opportunity};

    fn sample_opportunity() -> Opportunity {
        let token_start = Address::from_low_u64_be(1);
        let token_mid = Address::from_low_u64_be(2);
        Opportunity {
            first_pool: "pool <A&B>".to_string(),
            second_pool: "pool >B".to_string(),
            token_start,
            token_mid,
            amount_in: U256::from(1_000_u64),
            amount_after_first: U256::from(2_000_u64),
            amount_out: U256::from(1_100_u64),
            gross_profit_wei: 100,
            estimated_net_profit_wei: 80,
            profit_bps: 800,
            gas_adjusted: true,
            unix_ts: 1_700_000_000,
            legs: vec![ExecutionLeg {
                kind: 0,
                router: Address::from_low_u64_be(3),
                fee: 0,
                pool_id: H256::zero(),
                token_in: token_start,
                token_out: token_mid,
            }],
        }
    }

    #[test]
    fn telegram_message_formats_and_escapes_html() {
        let message = format_opportunity(&sample_opportunity());

        assert!(message.contains("🚀💰 <b>发现套利机会</b> 💰🚀"));
        assert!(message.contains("pool &lt;A&amp;B&gt; → pool &gt;B"));
        assert!(message.contains("<b>毛利润 wei</b>: 100"));
        assert!(message.contains("<b>预估净利润 wei</b>: 80"));
        assert!(message.contains("<b>利润 bps</b>: 800"));
        assert!(message.contains("<b>已扣 gas</b>: true"));
    }

    #[test]
    fn html_escape_handles_telegram_html_special_chars() {
        assert_eq!(
            html_escape("a < b && c > d"),
            "a &lt; b &amp;&amp; c &gt; d"
        );
    }

    #[tokio::test]
    #[ignore = "需要真实 TELEGRAM_BOT_TOKEN 和 TELEGRAM_CHAT_ID，会实际发送 Telegram 消息"]
    async fn sends_real_telegram_message_when_env_is_configured() -> eyre::Result<()> {
        dotenvy::dotenv().ok();
        let bot_token = match env::var("TELEGRAM_BOT_TOKEN") {
            Ok(value) if !value.trim().is_empty() && !value.contains("replace_me") => value,
            _ => {
                eprintln!("skip: TELEGRAM_BOT_TOKEN 未配置真实值");
                return Ok(());
            }
        };
        let chat_id = match env::var("TELEGRAM_CHAT_ID") {
            Ok(value) if !value.trim().is_empty() && value != "123456789" => value,
            _ => {
                eprintln!("skip: TELEGRAM_CHAT_ID 未配置真实值");
                return Ok(());
            }
        };

        send_telegram_message(
            &bot_token,
            &chat_id,
            format_opportunity(&sample_opportunity()),
        )
        .await
    }
}
