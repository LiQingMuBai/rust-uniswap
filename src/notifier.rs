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
