//! The Telegram channel: a bot visitors start from a `t.me` link.
//!
//! Signing up makes a pending row with no address and a confirm token; the
//! visitor opens `t.me/<bot>?start=<token>`, Telegram sends the bot
//! `/start <token>`, and the chat that sent it becomes the address. The bot
//! reads its messages by long polling `getUpdates`, so dunlin needs no
//! public webhook endpoint.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use sqlx::Row;
use tokio::sync::watch;

use super::dispatch::{Delivery, Message, Pacing, RetryAfter, SubscriberChannel, SubscriberGone};
use crate::config::{Config, SubscriberTelegramConfig};
use crate::db::{self, Pool};

pub const TELEGRAM_API: &str = "https://api.telegram.org";
/// How long one `getUpdates` call waits for news before returning empty.
pub const POLL_TIMEOUT_SECS: u64 = 30;
/// Telegram's limit on a message's text.
pub const MAX_TEXT: usize = 4096;
/// Telegram allows about 30 messages a second overall and one a second per
/// chat before it answers 429.
const GLOBAL_GAP: Duration = Duration::from_millis(34);
const PER_CHAT_GAP: Duration = Duration::from_secs(1);

/// A Bot API answer that was not `ok`, with what Telegram said. Never holds
/// the request URL, which contains the bot token.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub description: String,
    pub retry_after: Option<i64>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "telegram answered {}: {}", self.status, self.description)
    }
}

impl std::error::Error for ApiError {}

#[derive(Clone)]
pub struct BotApi {
    client: reqwest::Client,
    base: String,
    token: String,
}

impl BotApi {
    pub fn new(client: reqwest::Client, base: String, token: String) -> Self {
        Self {
            client,
            base,
            token,
        }
    }

    /// The bot's numeric id, the part of the token before the colon. It is
    /// public (it is in every message the bot sends).
    fn bot_id(&self) -> &str {
        self.token.split(':').next().unwrap_or("")
    }

    async fn call(
        &self,
        method: &str,
        body: &serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        let url = format!(
            "{}/bot{}/{method}",
            self.base.trim_end_matches('/'),
            self.token
        );
        // reqwest puts the URL in its errors, and the URL holds the token:
        // strip it before the error can reach a log.
        let resp = self
            .client
            .post(url)
            .timeout(timeout)
            .json(body)
            .send()
            .await
            .map_err(|e| anyhow!("telegram {method}: {}", e.without_url()))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| anyhow!("telegram {method}: {}", e.without_url()))?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("telegram {method}: answer is not JSON ({status})"))?;
        if v["ok"].as_bool() == Some(true) {
            return Ok(v["result"].clone());
        }
        Err(ApiError {
            status,
            description: v["description"]
                .as_str()
                .unwrap_or("no description")
                .to_string(),
            retry_after: v["parameters"]["retry_after"].as_i64(),
        }
        .into())
    }

    pub async fn get_me(&self) -> Result<String> {
        let me = self
            .call(
                "getMe",
                &serde_json::json!({}),
                crate::notifier::SEND_TIMEOUT,
            )
            .await?;
        me["username"]
            .as_str()
            .map(str::to_string)
            .context("telegram getMe: no username")
    }

    pub async fn send_message(&self, chat_id: &str, html: &str) -> Result<()> {
        self.call(
            "sendMessage",
            &serde_json::json!({
                "chat_id": chat_id,
                "text": html,
                "parse_mode": "HTML",
                "disable_web_page_preview": true,
            }),
            crate::notifier::SEND_TIMEOUT,
        )
        .await
        .map(|_| ())
    }

    pub async fn get_updates(
        &self,
        offset: i64,
        timeout_secs: u64,
    ) -> Result<Vec<serde_json::Value>> {
        let result = self
            .call(
                "getUpdates",
                &serde_json::json!({
                    "offset": offset,
                    "timeout": timeout_secs,
                    "allowed_updates": ["message"],
                }),
                // Telegram holds the request open for up to `timeout`.
                Duration::from_secs(timeout_secs + 10),
            )
            .await?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }
}

/// `<`, `>` and `&` are the only characters Telegram's HTML mode needs
/// escaped in text.
pub fn escape(s: &str) -> String {
    crate::notifier::slack_escape(s)
}

/// `s` escaped, cut so the escaped text fits `budget` characters. Cutting
/// the raw text, not the escaped one, never splits an `&amp;`.
fn escape_within(s: &str, budget: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let piece = escape(c.encode_utf8(&mut [0; 4]));
        let len = piece.chars().count();
        // Keep room for the ellipsis unless this is the last character.
        let reserve = usize::from(chars.peek().is_some());
        if used + len + reserve > budget {
            out.push('…');
            return out;
        }
        used += len;
        out.push_str(&piece);
    }
    out
}

fn unsubscribe_url(d: &Delivery) -> &str {
    match &d.message {
        Message::Confirm {
            unsubscribe_url, ..
        }
        | Message::Event {
            unsubscribe_url, ..
        } => unsubscribe_url,
    }
}

/// The message as Telegram HTML: the email wording, subject in bold, and
/// how to stop at the end, all within Telegram's length limit.
pub fn render(d: &Delivery) -> String {
    let (subject, body) = super::email::compose(d);
    let head = format!(
        "<b>{}</b>\n\n",
        escape(&crate::notifier::truncate_chars(&subject, 256))
    );
    let foot = format!(
        "\n\nUnsubscribe: send /stop or open {}",
        escape(unsubscribe_url(d))
    );
    let budget = MAX_TEXT.saturating_sub(head.chars().count() + foot.chars().count());
    format!("{head}{}{foot}", escape_within(body.trim_end(), budget))
}

pub struct TelegramChannel {
    api: BotApi,
    /// From the config, or asked of Telegram once and kept.
    username: tokio::sync::OnceCell<String>,
}

impl TelegramChannel {
    pub fn new(cfg: &SubscriberTelegramConfig) -> Result<Self> {
        Ok(Self::with_api(
            BotApi::new(client()?, TELEGRAM_API.to_string(), cfg.token.clone()),
            cfg.username.clone(),
        ))
    }

    /// Against any Bot API server; tests use a local mock.
    pub fn with_api(api: BotApi, username: Option<String>) -> Self {
        let cell = tokio::sync::OnceCell::new();
        if let Some(u) = username {
            let _ = cell.set(u);
        }
        Self {
            api,
            username: cell,
        }
    }
}

pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("dunlin/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the telegram client")
}

/// Map a failed `sendMessage` to what the dispatcher should do. Telegram's
/// descriptions are matched narrowly, on the texts it documents for a chat
/// that is gone for good; anything else is retried as usual.
pub fn classify(e: anyhow::Error) -> anyhow::Error {
    let Some(api) = e.downcast_ref::<ApiError>() else {
        return e;
    };
    let d = api.description.to_ascii_lowercase();
    let gone = match api.status {
        403 => {
            d.contains("bot was blocked by the user")
                || d.contains("user is deactivated")
                || d.contains("bot was kicked")
        }
        400 => d.contains("chat not found"),
        _ => false,
    };
    if gone {
        return SubscriberGone(api.to_string()).into();
    }
    if api.status == 429 {
        return RetryAfter {
            secs: api.retry_after.unwrap_or(1),
            message: api.to_string(),
        }
        .into();
    }
    e
}

#[async_trait]
impl SubscriberChannel for TelegramChannel {
    fn name(&self) -> &'static str {
        "telegram"
    }

    fn label(&self) -> &str {
        "Telegram"
    }

    fn address_hint(&self) -> &str {
        "no address needed, you get a link to the bot"
    }

    fn normalize_address(&self, _input: &str) -> std::result::Result<String, String> {
        Err("Telegram subscriptions start from the bot link.".to_string())
    }

    fn needs_address(&self) -> bool {
        false
    }

    async fn start_link(&self) -> Option<String> {
        let username = self
            .username
            .get_or_try_init(|| self.api.get_me())
            .await
            .map_err(|e| tracing::warn!(error = %e, "telegram getMe failed"))
            .ok()?;
        Some(format!("https://t.me/{username}?start="))
    }

    fn pacing(&self) -> Pacing {
        Pacing {
            global: GLOBAL_GAP,
            per_address: PER_CHAT_GAP,
        }
    }

    async fn send(&self, d: &Delivery) -> Result<()> {
        self.api
            .send_message(&d.address, &render(d))
            .await
            .map_err(classify)
    }
}

// -- the bot -----------------------------------------------------------------

/// What happened to a `/start` link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Linked {
    Subscribed {
        all_components: bool,
        components: Vec<String>,
        maintenance: bool,
    },
    Expired,
    Invalid,
}

/// Tie a pending sign-up to the chat that opened its link. A chat already
/// subscribed takes the new choices instead of getting a second row: it
/// proved it is the same chat by pressing Start, unlike an email address
/// typed into the form, which anyone could type.
pub async fn link_chat(pool: &Pool, token: &str, chat: &str, now: i64) -> Result<Linked> {
    let mut tx = pool.begin().await?;
    let Some(row) = sqlx::query(
        "SELECT id, confirm_expires, all_components, maintenance FROM subscribers
         WHERE channel = 'telegram' AND status = 'pending' AND confirm_hash = ?",
    )
    .bind(crate::auth::token_hash(token))
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Linked::Invalid);
    };
    let pending: i64 = row.get("id");
    if row
        .get::<Option<i64>, _>("confirm_expires")
        .is_none_or(|e| e <= now)
    {
        return Ok(Linked::Expired);
    }
    let all_components = row.get::<i64, _>("all_components") != 0;
    let maintenance = row.get::<i64, _>("maintenance") != 0;
    let components: Vec<String> =
        sqlx::query("SELECT component_id FROM subscriber_components WHERE subscriber_id = ? ORDER BY component_id")
            .bind(pending)
            .fetch_all(&mut *tx)
            .await?
            .iter()
            .map(|r| r.get("component_id"))
            .collect();

    let existing: Option<i64> =
        sqlx::query("SELECT id FROM subscribers WHERE channel = 'telegram' AND address = ?")
            .bind(chat)
            .fetch_optional(&mut *tx)
            .await?
            .map(|r| r.get("id"));
    match existing {
        Some(id) => {
            sqlx::query(
                "UPDATE subscribers SET all_components = ?, maintenance = ?, status = 'active',
                   confirmed_at = ?, failure_window_start = NULL, failures_in_window = 0,
                   quarantined_at = NULL
                 WHERE id = ?",
            )
            .bind(all_components as i64)
            .bind(maintenance as i64)
            .bind(now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM subscriber_components WHERE subscriber_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE subscriber_components SET subscriber_id = ? WHERE subscriber_id = ?",
            )
            .bind(id)
            .bind(pending)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM subscribers WHERE id = ?")
                .bind(pending)
                .execute(&mut *tx)
                .await?;
        }
        None => {
            sqlx::query(
                "UPDATE subscribers SET address = ?, status = 'active', confirmed_at = ?,
                   confirm_hash = NULL, confirm_expires = NULL
                 WHERE id = ?",
            )
            .bind(chat)
            .bind(now)
            .bind(pending)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(Linked::Subscribed {
        all_components,
        components,
        maintenance,
    })
}

/// Unsubscribe a chat. Whether it was subscribed.
pub async fn stop_chat(pool: &Pool, chat: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM subscribers WHERE channel = 'telegram' AND address = ?")
        .bind(chat)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

fn subscribe_url(cfg: &Config) -> String {
    format!(
        "{}/subscribe",
        cfg.public_url
            .as_deref()
            .unwrap_or("")
            .trim_end_matches('/')
    )
}

/// A start payload can only be a token we made: 64 hex characters. Anything
/// else is not looked up at all.
fn plausible_token(p: &str) -> bool {
    p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The reply to one message, if it is a command the bot knows. The
/// sender's text is never repeated back.
async fn answer(
    pool: &Pool,
    cfg: &Config,
    chat: &str,
    text: &str,
    now: i64,
) -> Result<Option<String>> {
    let mut words = text.split_whitespace();
    // In groups commands come as "/start@bot_name".
    let command = words.next().unwrap_or("").split('@').next().unwrap_or("");
    let site = escape(cfg.theme.title.trim());
    let signup = escape(&subscribe_url(cfg));
    Ok(Some(match (command, words.next()) {
        ("/start", None) => {
            format!("To get updates from <b>{site}</b>, choose what to follow at {signup}")
        }
        ("/start", Some(p)) => {
            let linked = if plausible_token(p) {
                link_chat(pool, p, chat, now).await?
            } else {
                Linked::Invalid
            };
            match linked {
                Linked::Subscribed {
                    all_components,
                    components,
                    maintenance,
                } => {
                    let about = if all_components {
                        "all components".to_string()
                    } else {
                        components
                            .iter()
                            .map(|c| escape(&super::component_label(cfg, c)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    let extra = if maintenance { ", and maintenance" } else { "" };
                    format!(
                        "Subscribed to updates from <b>{site}</b> about {about}{extra}.\nSend /stop to unsubscribe."
                    )
                }
                Linked::Expired => format!(
                    "This link has expired; links work for 24 hours. Subscribe again at {signup}"
                ),
                Linked::Invalid => format!(
                    "This link is not valid or was already used. Subscribe again at {signup}"
                ),
            }
        }
        ("/stop" | "/unsubscribe", _) => {
            if stop_chat(pool, chat).await? {
                "Unsubscribed. This chat gets no more updates.".to_string()
            } else {
                "This chat is not subscribed.".to_string()
            }
        }
        _ => return Ok(None),
    }))
}

/// Where the next `getUpdates` starts, per bot, so a restart does not
/// handle a message twice and a new bot does not inherit an old offset.
fn offset_key(api: &BotApi) -> String {
    format!("telegram_offset:{}", api.bot_id())
}

/// One `getUpdates` round: answer each message, then move the offset past
/// it. Returns how many updates came in.
pub async fn poll_once(
    pool: &Pool,
    cfg: &Config,
    api: &BotApi,
    timeout_secs: u64,
) -> Result<usize> {
    let key = offset_key(api);
    let offset = db::get_meta(pool, &key)
        .await?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let updates = api.get_updates(offset, timeout_secs).await?;
    for u in &updates {
        let Some(id) = u["update_id"].as_i64() else {
            continue;
        };
        let msg = &u["message"];
        let chat = msg["chat"]["id"].as_i64();
        let text = msg["text"].as_str();
        if let (Some(chat), Some(text)) = (chat, text) {
            let chat = chat.to_string();
            match answer(pool, cfg, &chat, text, crate::now_ts()).await {
                Ok(Some(reply)) => {
                    if let Err(e) = api.send_message(&chat, &reply).await {
                        tracing::warn!(error = %e, "telegram reply failed");
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "telegram command failed"),
            }
        }
        db::set_meta(pool, &key, &(id + 1).to_string()).await?;
    }
    Ok(updates.len())
}

/// How long to wait after a failed round. A 409 means something else reads
/// this bot's updates (another program polling it, or a webhook set on it):
/// that will not fix itself quickly, so the log says what to do and the
/// wait is long.
pub fn after_error(e: &anyhow::Error) -> Duration {
    match e.downcast_ref::<ApiError>() {
        Some(api) if api.status == 409 => Duration::from_secs(60),
        Some(api) if api.status == 429 => {
            Duration::from_secs(api.retry_after.unwrap_or(5).clamp(1, 300) as u64)
        }
        _ => Duration::from_secs(10),
    }
}

/// The bot's polling task. Idle while Telegram subscriptions are off.
pub async fn poll_loop(pool: Pool, config_rx: watch::Receiver<Arc<Config>>) {
    let client = match client() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "telegram subscriptions disabled");
            return;
        }
    };
    loop {
        let cfg = config_rx.borrow().clone();
        let telegram = cfg
            .subscriptions
            .telegram
            .as_ref()
            .filter(|t| t.enabled && cfg.subscriptions.enabled);
        let Some(t) = telegram else {
            tokio::time::sleep(Duration::from_secs(30)).await;
            continue;
        };
        let api = BotApi::new(client.clone(), TELEGRAM_API.to_string(), t.token.clone());
        if let Err(e) = poll_once(&pool, &cfg, &api, POLL_TIMEOUT_SECS).await {
            if e.downcast_ref::<ApiError>()
                .is_some_and(|a| a.status == 409)
            {
                tracing::warn!(
                    error = %e,
                    "telegram bot updates are taken by another poller or a webhook; give subscriptions a bot of their own"
                );
            } else {
                tracing::warn!(error = %e, "telegram polling failed");
            }
            tokio::time::sleep(after_error(&e)).await;
        }
    }
}

#[cfg(test)]
mod tests;
