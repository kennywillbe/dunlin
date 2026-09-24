//! Notification channels behind a `Notifier` trait.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::config::{Config, NotifierConfig};
use crate::models::{Notification, State};

#[async_trait]
pub trait Notifier: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, notification: &Notification) -> Result<()>;
}

/// How long one request to a channel may take. Alerts are sent
/// from the probe loop, so a channel that never answers must not hold it.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Fan-out to every configured channel; one broken channel never stops others.
/// The channel list can be swapped at runtime when the config is reloaded.
pub struct MultiNotifier {
    inner: RwLock<Vec<Arc<dyn Notifier>>>,
}

impl MultiNotifier {
    pub fn new(inner: Vec<Arc<dyn Notifier>>) -> Self {
        Self {
            inner: RwLock::new(inner),
        }
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Replace the channels; sends already under way finish on the old ones.
    pub fn replace(&self, inner: Vec<Arc<dyn Notifier>>) {
        *self.inner.write().unwrap() = inner;
    }

    /// Send to every channel at once, so the slowest one sets the wait.
    pub async fn send(&self, notification: &Notification) {
        let channels = self.inner.read().unwrap().clone();
        let sends = channels.iter().map(|n| async move {
            if let Err(e) = n.send(notification).await {
                tracing::warn!(notifier = n.name(), error = %e, "notification failed");
            }
        });
        futures_util::future::join_all(sends).await;
    }
}

pub struct TelegramNotifier {
    client: reqwest::Client,
    token: String,
    chat_id: String,
    timeout: Duration,
}

impl TelegramNotifier {
    pub fn new(client: reqwest::Client, token: String, chat_id: String) -> Self {
        Self {
            client,
            token,
            chat_id,
            timeout: SEND_TIMEOUT,
        }
    }
}

#[async_trait]
impl Notifier for TelegramNotifier {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn send(&self, notification: &Notification) -> Result<()> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let text = format!("{}\n{}", notification.title, notification.message);
        let resp = self
            .client
            .post(&url)
            .timeout(self.timeout)
            .json(&serde_json::json!({
                "chat_id": self.chat_id,
                "text": text,
                "disable_web_page_preview": true,
            }))
            .send()
            .await
            .context("telegram request")?;
        if !resp.status().is_success() {
            anyhow::bail!("telegram returned {}", resp.status());
        }
        Ok(())
    }
}

/// JSON body sent to webhook notifiers. Documented in the README.
#[derive(Debug, serde::Serialize)]
pub struct WebhookPayload<'a> {
    pub event: &'a str,
    pub title: &'a str,
    pub message: &'a str,
    pub component: &'a str,
    pub state: &'a str,
    pub incident_id: Option<i64>,
    pub timestamp: i64,
}

impl<'a> From<&'a Notification> for WebhookPayload<'a> {
    fn from(n: &'a Notification) -> Self {
        Self {
            event: &n.event,
            title: &n.title,
            message: &n.message,
            component: &n.component,
            state: n.state.as_str(),
            incident_id: n.incident_id,
            timestamp: chrono::Utc::now().timestamp(),
        }
    }
}

pub struct WebhookNotifier {
    client: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    timeout: Duration,
}

impl WebhookNotifier {
    pub fn new(client: reqwest::Client, url: String, headers: BTreeMap<String, String>) -> Self {
        Self {
            client,
            url,
            headers,
            timeout: SEND_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl Notifier for WebhookNotifier {
    fn name(&self) -> &str {
        "webhook"
    }

    async fn send(&self, notification: &Notification) -> Result<()> {
        let mut req = self
            .client
            .post(&self.url)
            .timeout(self.timeout)
            .json(&WebhookPayload::from(notification));
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.context("webhook request")?;
        if !resp.status().is_success() {
            anyhow::bail!("webhook returned {}", resp.status());
        }
        Ok(())
    }
}

/// Cut `s` to at most `max` bytes on a char boundary, ending in "…" when cut.
fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Cut `s` to at most `max` chars, ending in "…" when cut. Discord and
/// Pushover count their limits in characters, not bytes.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Colour of a state, taken from the status page palette. Operational has no
/// colour there (it is the calm default), so notifications use a green.
fn state_rgb(state: State) -> u32 {
    match state {
        State::Operational => 0x2e9e5b,
        State::Maintenance => 0x2f64d8,
        State::Degraded => 0xe0a21b,
        State::PartialOutage => 0xe2461f,
        State::MajorOutage => 0xb8121d,
    }
}

/// Longest wait honoured on a 429 before the single retry.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(5);

/// How long a 429 asks us to wait: Discord's `retry_after` body field, else
/// the `Retry-After` header, both in (possibly fractional) seconds.
async fn retry_wait(resp: reqwest::Response) -> Duration {
    let header = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok());
    let body = resp
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v.get("retry_after").and_then(|r| r.as_f64()));
    clamp_wait(body.or(header))
}

fn clamp_wait(secs: Option<f64>) -> Duration {
    // A 429 without a usable hint still gets a short pause, not a hammering.
    let wait = secs
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
        .unwrap_or(Duration::from_secs(1));
    wait.min(MAX_RETRY_WAIT)
}

/// POST `body` as JSON, retrying once if rate limited. Discord and Slack
/// both answer bursts (an outage hitting many checks) with 429.
async fn post_json_retry_once(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    timeout: Duration,
    name: &str,
) -> Result<()> {
    let mut retried = false;
    loop {
        let resp = client
            .post(url)
            .timeout(timeout)
            .json(body)
            .send()
            .await
            .with_context(|| format!("{name} request"))?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && !retried {
            retried = true;
            tokio::time::sleep(retry_wait(resp).await).await;
            continue;
        }
        if !resp.status().is_success() {
            anyhow::bail!("{name} returned {}", resp.status());
        }
        return Ok(());
    }
}

/// Publishes to an ntfy topic with the JSON API.
pub struct NtfyNotifier {
    client: reqwest::Client,
    url: String,
    topic: String,
    token: Option<String>,
    timeout: Duration,
}

impl NtfyNotifier {
    pub fn new(client: reqwest::Client, url: String, topic: String, token: Option<String>) -> Self {
        Self {
            client,
            url,
            topic,
            token,
            timeout: SEND_TIMEOUT,
        }
    }

    fn payload(&self, n: &Notification) -> serde_json::Value {
        let (priority, tag) = match n.event.as_str() {
            "down" => (5, Some("rotating_light")),
            "reminder" => (4, Some("warning")),
            "up" => (3, Some("white_check_mark")),
            "summary" => (2, Some("bar_chart")),
            _ => (3, None),
        };
        let mut body = serde_json::json!({
            "topic": self.topic,
            "title": truncate_bytes(&n.title, 1024),
            "message": truncate_bytes(n.text(), 4096),
            "priority": priority,
            "tags": tag.into_iter().collect::<Vec<_>>(),
        });
        if let Some(link) = &n.link {
            body["click"] = link.as_str().into();
        }
        body
    }
}

#[async_trait]
impl Notifier for NtfyNotifier {
    fn name(&self) -> &str {
        "ntfy"
    }

    async fn send(&self, notification: &Notification) -> Result<()> {
        // JSON publishing goes to the server root; the topic is in the body.
        let mut req = self
            .client
            .post(&self.url)
            .timeout(self.timeout)
            .json(&self.payload(notification));
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.context("ntfy request")?;
        if !resp.status().is_success() {
            anyhow::bail!("ntfy returned {}", resp.status());
        }
        Ok(())
    }
}

/// Posts one embed to a Discord webhook.
pub struct DiscordNotifier {
    client: reqwest::Client,
    url: String,
    timeout: Duration,
}

impl DiscordNotifier {
    pub fn new(client: reqwest::Client, url: String) -> Self {
        Self {
            client,
            url,
            timeout: SEND_TIMEOUT,
        }
    }

    fn payload(n: &Notification) -> serde_json::Value {
        // Title (256) plus description (4096) stays under the 6000 total.
        let mut embed = serde_json::json!({
            "title": truncate_chars(&n.title, 256),
            "description": truncate_chars(n.text(), 4096),
            "color": state_rgb(n.state),
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        });
        if let Some(link) = &n.link {
            embed["url"] = link.as_str().into();
        }
        serde_json::json!({
            "embeds": [embed],
            // Check output is untrusted text; "@everyone" in it must not ping.
            "allowed_mentions": { "parse": [] },
        })
    }
}

#[async_trait]
impl Notifier for DiscordNotifier {
    fn name(&self) -> &str {
        "discord"
    }

    async fn send(&self, notification: &Notification) -> Result<()> {
        let body = Self::payload(notification);
        post_json_retry_once(&self.client, &self.url, &body, self.timeout, "discord").await
    }
}

/// Posts a coloured legacy attachment to a Slack incoming webhook.
pub struct SlackNotifier {
    client: reqwest::Client,
    url: String,
    timeout: Duration,
}

/// Slack reads `&`, `<` and `>` as markup; `&` goes first so the entities
/// added for the other two are not escaped again.
fn slack_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

impl SlackNotifier {
    pub fn new(client: reqwest::Client, url: String) -> Self {
        Self {
            client,
            url,
            timeout: SEND_TIMEOUT,
        }
    }

    fn payload(n: &Notification) -> serde_json::Value {
        let title = slack_escape(&n.title);
        let mut attachment = serde_json::json!({
            "color": format!("#{:06x}", state_rgb(n.state)),
            "title": title,
            "text": slack_escape(n.text()),
            "ts": chrono::Utc::now().timestamp(),
        });
        if let Some(link) = &n.link {
            attachment["title_link"] = link.as_str().into();
        }
        serde_json::json!({
            // Shown in push notifications, where attachments are not.
            "text": title,
            "attachments": [attachment],
        })
    }
}

#[async_trait]
impl Notifier for SlackNotifier {
    fn name(&self) -> &str {
        "slack"
    }

    async fn send(&self, notification: &Notification) -> Result<()> {
        let body = Self::payload(notification);
        post_json_retry_once(&self.client, &self.url, &body, self.timeout, "slack").await
    }
}

pub const PUSHOVER_API: &str = "https://api.pushover.net";

/// Sends through the Pushover message API.
pub struct PushoverNotifier {
    client: reqwest::Client,
    base_url: String,
    token: String,
    user: String,
    timeout: Duration,
}

impl PushoverNotifier {
    pub fn new(client: reqwest::Client, token: String, user: String) -> Self {
        Self {
            client,
            base_url: PUSHOVER_API.to_string(),
            token,
            user,
            timeout: SEND_TIMEOUT,
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    fn form(&self, n: &Notification) -> Vec<(&'static str, String)> {
        // Priority 2 would demand acknowledgement; a down alert is loud
        // enough at 1, which also bypasses quiet hours.
        let priority = if n.event == "down" { "1" } else { "0" };
        let mut form = vec![
            ("token", self.token.clone()),
            ("user", self.user.clone()),
            ("title", truncate_chars(&n.title, 250)),
            ("message", truncate_chars(n.text(), 1024)),
            ("priority", priority.to_string()),
        ];
        // A cut URL would lead nowhere, so a long one is left out instead.
        if let Some(link) = n.link.as_ref().filter(|l| l.chars().count() <= 512) {
            form.push(("url", link.clone()));
            form.push(("url_title", "View incident".to_string()));
        }
        form
    }
}

#[async_trait]
impl Notifier for PushoverNotifier {
    fn name(&self) -> &str {
        "pushover"
    }

    async fn send(&self, notification: &Notification) -> Result<()> {
        let url = format!("{}/1/messages.json", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .timeout(self.timeout)
            .form(&self.form(notification))
            .send()
            .await
            .context("pushover request")?;
        if !resp.status().is_success() {
            anyhow::bail!("pushover returned {}", resp.status());
        }
        Ok(())
    }
}

/// Records notifications in memory; used by tests and `MultiNotifier` tests.
#[derive(Default)]
pub struct RecordingNotifier {
    pub sent: Mutex<Vec<Notification>>,
}

impl RecordingNotifier {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }

    pub fn messages(&self) -> Vec<Notification> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl Notifier for RecordingNotifier {
    fn name(&self) -> &str {
        "recording"
    }

    async fn send(&self, notification: &Notification) -> Result<()> {
        self.sent.lock().unwrap().push(notification.clone());
        Ok(())
    }
}

/// Build the configured channels.
pub fn build_notifiers(cfg: &Config, client: &reqwest::Client) -> Vec<Arc<dyn Notifier>> {
    cfg.notifiers
        .iter()
        .map(|n| match n {
            NotifierConfig::Telegram { token, chat_id } => Arc::new(TelegramNotifier::new(
                client.clone(),
                token.clone(),
                chat_id.clone(),
            )) as Arc<dyn Notifier>,
            NotifierConfig::Webhook { url, headers } => Arc::new(WebhookNotifier::new(
                client.clone(),
                url.clone(),
                headers.clone(),
            )) as Arc<dyn Notifier>,
            NotifierConfig::Ntfy { url, topic, token } => Arc::new(NtfyNotifier::new(
                client.clone(),
                url.clone(),
                topic.clone(),
                token.clone(),
            )) as Arc<dyn Notifier>,
            NotifierConfig::Discord { url } => {
                Arc::new(DiscordNotifier::new(client.clone(), url.clone())) as Arc<dyn Notifier>
            }
            NotifierConfig::Slack { url } => {
                Arc::new(SlackNotifier::new(client.clone(), url.clone())) as Arc<dyn Notifier>
            }
            NotifierConfig::Pushover { token, user } => Arc::new(PushoverNotifier::new(
                client.clone(),
                token.clone(),
                user.clone(),
            )) as Arc<dyn Notifier>,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use std::collections::VecDeque;

    fn sample() -> Notification {
        Notification {
            event: "alert".into(),
            title: "Web is down".into(),
            message: "connection refused".into(),
            component: "web".into(),
            state: State::MajorOutage,
            incident_id: Some(7),
            link: None,
        }
    }

    /// A down alert as the alert engine sends it with `public_url` set.
    fn linked_down() -> Notification {
        let link = "https://status.example.org/incidents/7".to_string();
        Notification {
            event: "down".into(),
            message: format!("connection refused\n{link}"),
            link: Some(link),
            ..sample()
        }
    }

    struct Hit {
        path: String,
        headers: HeaderMap,
        body: String,
    }

    impl Hit {
        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.body).unwrap()
        }
    }

    type Hits = Arc<Mutex<Vec<Hit>>>;

    /// Answers with `replies` in order, then 200, recording every request.
    async fn mock(replies: Vec<Response>) -> (String, Hits) {
        let hits: Hits = Arc::default();
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let recorded = hits.clone();
        let app = axum::Router::new().fallback(
            move |uri: axum::http::Uri, headers: HeaderMap, body: String| {
                let hits = recorded.clone();
                let replies = replies.clone();
                async move {
                    hits.lock().unwrap().push(Hit {
                        path: uri.path().to_string(),
                        headers,
                        body,
                    });
                    let next = replies.lock().unwrap().pop_front();
                    next.unwrap_or_else(|| "ok".into_response())
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), hits)
    }

    fn too_many(retry_after_header: Option<&str>, body: &str) -> Response {
        let mut resp = (StatusCode::TOO_MANY_REQUESTS, body.to_string()).into_response();
        if let Some(v) = retry_after_header {
            resp.headers_mut().insert("retry-after", v.parse().unwrap());
        }
        resp
    }

    #[tokio::test]
    async fn recording_notifier_records() {
        let n = RecordingNotifier::new();
        n.send(&sample()).await.unwrap();
        assert_eq!(n.count(), 1);
        assert_eq!(n.messages()[0].component, "web");
    }

    #[tokio::test]
    async fn multi_notifier_fans_out() {
        let a = RecordingNotifier::new();
        let b = RecordingNotifier::new();
        let multi = MultiNotifier::new(vec![a.clone(), b.clone()]);
        multi.send(&sample()).await;
        assert_eq!(a.count(), 1);
        assert_eq!(b.count(), 1);
    }

    #[tokio::test]
    async fn replaced_channels_get_later_sends() {
        let a = RecordingNotifier::new();
        let b = RecordingNotifier::new();
        let multi = MultiNotifier::new(vec![a.clone()]);
        multi.send(&sample()).await;
        multi.replace(vec![b.clone()]);
        multi.send(&sample()).await;
        assert_eq!((a.count(), b.count()), (1, 1));
    }

    #[tokio::test]
    async fn webhook_that_never_answers_times_out() {
        // Accepts the connection, then never writes a response.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        let n = WebhookNotifier::new(
            reqwest::Client::new(),
            format!("http://{addr}/hook"),
            BTreeMap::new(),
        )
        .with_timeout(Duration::from_millis(200));
        let started = std::time::Instant::now();
        assert!(n.send(&sample()).await.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn webhook_payload_shape_against_mock() {
        use axum::{extract::State, routing::post, Router};

        let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let state = captured.clone();
        let app = Router::new()
            .route(
                "/hook",
                post(
                    |State(c): State<Arc<Mutex<Vec<serde_json::Value>>>>, body: String| async move {
                        c.lock().unwrap().push(serde_json::from_str(&body).unwrap());
                        "ok"
                    },
                ),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let n = WebhookNotifier::new(client, format!("http://{addr}/hook"), BTreeMap::new());
        n.send(&sample()).await.unwrap();

        let body = captured.lock().unwrap()[0].clone();
        assert_eq!(body["event"], "alert");
        assert_eq!(body["component"], "web");
        assert_eq!(body["state"], "major_outage");
        assert_eq!(body["incident_id"], 7);
        assert!(body["timestamp"].as_i64().unwrap() > 0);
    }

    #[test]
    fn truncation_keeps_char_boundaries() {
        assert_eq!(truncate_bytes("short", 10), "short");
        // "ü" is two bytes, so the room left after "…" ends mid-char.
        let cut = truncate_bytes("aüüüü", 7);
        assert!(cut.len() <= 7, "{cut}");
        assert_eq!(cut, "aü…");
        assert_eq!(truncate_chars("ağaç", 4), "ağaç");
        assert_eq!(truncate_chars("ağaçlar", 4), "ağa…");
    }

    #[test]
    fn text_drops_only_the_trailing_link() {
        assert_eq!(linked_down().text(), "connection refused");
        assert_eq!(sample().text(), "connection refused");
    }

    #[test]
    fn retry_wait_is_capped() {
        assert_eq!(clamp_wait(Some(0.25)), Duration::from_millis(250));
        assert_eq!(clamp_wait(Some(7.5)), Duration::from_secs(5));
        assert_eq!(clamp_wait(Some(3600.0)), Duration::from_secs(5));
        assert_eq!(clamp_wait(Some(-1.0)), Duration::from_secs(1));
        assert_eq!(clamp_wait(None), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn ntfy_payload_against_mock() {
        let (base, hits) = mock(vec![]).await;
        let n = NtfyNotifier::new(
            reqwest::Client::new(),
            base,
            "alerts".into(),
            Some("tk_secret".into()),
        );
        n.send(&linked_down()).await.unwrap();
        let summary = Notification {
            event: "summary".into(),
            message: "x".repeat(5000),
            title: "é".repeat(600),
            incident_id: None,
            ..sample()
        };
        n.send(&summary).await.unwrap();

        let hits = hits.lock().unwrap();
        assert_eq!(hits[0].path, "/");
        assert_eq!(hits[0].headers["authorization"], "Bearer tk_secret");
        let down = hits[0].json();
        assert_eq!(down["topic"], "alerts");
        assert_eq!(down["title"], "Web is down");
        assert_eq!(down["message"], "connection refused");
        assert_eq!(down["priority"], 5);
        assert_eq!(down["tags"], serde_json::json!(["rotating_light"]));
        assert_eq!(down["click"], "https://status.example.org/incidents/7");

        let summary = hits[1].json();
        assert_eq!(summary["priority"], 2);
        assert_eq!(summary["tags"], serde_json::json!(["bar_chart"]));
        assert!(summary.get("click").is_none());
        assert!(summary["message"].as_str().unwrap().len() <= 4096);
        assert!(summary["title"].as_str().unwrap().len() <= 1024);
    }

    #[tokio::test]
    async fn ntfy_priority_follows_the_event() {
        let (base, hits) = mock(vec![]).await;
        let n = NtfyNotifier::new(reqwest::Client::new(), base, "a".into(), None);
        for event in ["reminder", "up"] {
            let note = Notification {
                event: event.into(),
                ..sample()
            };
            n.send(&note).await.unwrap();
        }
        let hits = hits.lock().unwrap();
        assert!(!hits[0].headers.contains_key("authorization"));
        assert_eq!(hits[0].json()["priority"], 4);
        assert_eq!(hits[0].json()["tags"], serde_json::json!(["warning"]));
        assert_eq!(hits[1].json()["priority"], 3);
        assert_eq!(
            hits[1].json()["tags"],
            serde_json::json!(["white_check_mark"])
        );
    }

    #[tokio::test]
    async fn discord_payload_against_mock() {
        let (base, hits) = mock(vec![]).await;
        let n = DiscordNotifier::new(reqwest::Client::new(), format!("{base}/api/webhooks/1/x"));
        let noisy = Notification {
            title: "t".repeat(300),
            message: "@everyone ".repeat(500),
            state: State::Degraded,
            ..linked_down()
        };
        n.send(&linked_down()).await.unwrap();
        n.send(&noisy).await.unwrap();

        let hits = hits.lock().unwrap();
        assert_eq!(hits[0].path, "/api/webhooks/1/x");
        let body = hits[0].json();
        assert_eq!(body["allowed_mentions"], serde_json::json!({ "parse": [] }));
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"], "Web is down");
        assert_eq!(embed["description"], "connection refused");
        assert_eq!(embed["color"], 0xb8121d);
        assert_eq!(embed["url"], "https://status.example.org/incidents/7");
        let ts = embed["timestamp"].as_str().unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(ts).is_ok(), "{ts}");

        let embed = &hits[1].json()["embeds"][0];
        assert_eq!(embed["color"], 0xe0a21b);
        assert_eq!(embed["title"].as_str().unwrap().chars().count(), 256);
        assert_eq!(embed["description"].as_str().unwrap().chars().count(), 4096);
    }

    #[tokio::test]
    async fn discord_retries_once_after_retry_after() {
        let limited = || {
            too_many(
                None,
                r#"{"message":"rate limited","retry_after":0.05,"global":false}"#,
            )
        };
        let (base, hits) = mock(vec![limited()]).await;
        let n = DiscordNotifier::new(reqwest::Client::new(), base.clone());
        n.send(&sample()).await.unwrap();
        assert_eq!(hits.lock().unwrap().len(), 2);

        let (base, hits) = mock(vec![limited(), limited()]).await;
        let n = DiscordNotifier::new(reqwest::Client::new(), base);
        let err = n.send(&sample()).await.unwrap_err().to_string();
        assert!(err.contains("429"), "{err}");
        assert_eq!(hits.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn slack_payload_against_mock() {
        let (base, hits) = mock(vec![]).await;
        let n = SlackNotifier::new(reqwest::Client::new(), format!("{base}/services/x"));
        let tricky = Notification {
            title: "A & B <down>".into(),
            message: "got <!channel> & 5 > 3\nhttps://status.example.org/incidents/7".into(),
            ..linked_down()
        };
        n.send(&tricky).await.unwrap();
        n.send(&Notification {
            state: State::Maintenance,
            ..sample()
        })
        .await
        .unwrap();

        let hits = hits.lock().unwrap();
        assert_eq!(hits[0].path, "/services/x");
        let body = hits[0].json();
        assert_eq!(body["text"], "A &amp; B &lt;down&gt;");
        let att = &body["attachments"][0];
        assert_eq!(att["color"], "#b8121d");
        assert_eq!(att["title"], "A &amp; B &lt;down&gt;");
        assert_eq!(att["title_link"], "https://status.example.org/incidents/7");
        assert_eq!(att["text"], "got &lt;!channel&gt; &amp; 5 &gt; 3");
        assert!(att["ts"].as_i64().unwrap() > 0);

        let att = &hits[1].json()["attachments"][0];
        assert_eq!(att["color"], "#2f64d8");
        assert!(att.get("title_link").is_none());
    }

    #[tokio::test]
    async fn slack_retries_once_after_retry_after() {
        let (base, hits) = mock(vec![too_many(Some("0"), "rate_limited")]).await;
        let n = SlackNotifier::new(reqwest::Client::new(), base);
        n.send(&sample()).await.unwrap();
        assert_eq!(hits.lock().unwrap().len(), 2);

        let (base, hits) = mock(vec![
            too_many(Some("0"), "rate_limited"),
            too_many(Some("0"), "rate_limited"),
        ])
        .await;
        let n = SlackNotifier::new(reqwest::Client::new(), base);
        assert!(n.send(&sample()).await.is_err());
        assert_eq!(hits.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn pushover_form_against_mock() {
        let (base, hits) = mock(vec![]).await;
        let n = PushoverNotifier::new(reqwest::Client::new(), "app".into(), "me".into())
            .with_base_url(base);
        n.send(&linked_down()).await.unwrap();
        let long = Notification {
            event: "reminder".into(),
            title: "t".repeat(300),
            message: "m".repeat(2000),
            link: Some(format!("https://example.org/{}", "p".repeat(600))),
            ..sample()
        };
        n.send(&long).await.unwrap();

        let hits = hits.lock().unwrap();
        assert_eq!(hits[0].path, "/1/messages.json");
        let form: BTreeMap<String, String> = url::form_urlencoded::parse(hits[0].body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(form["token"], "app");
        assert_eq!(form["user"], "me");
        assert_eq!(form["title"], "Web is down");
        assert_eq!(form["message"], "connection refused");
        assert_eq!(form["priority"], "1");
        assert_eq!(form["url"], "https://status.example.org/incidents/7");
        assert_eq!(form["url_title"], "View incident");

        let form: BTreeMap<String, String> = url::form_urlencoded::parse(hits[1].body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(form["priority"], "0");
        assert_eq!(form["title"].chars().count(), 250);
        assert_eq!(form["message"].chars().count(), 1024);
        assert!(!form.contains_key("url"));
    }

    #[test]
    fn every_notifier_type_is_built() {
        let cfg = crate::config::parse_str(&format!(
            r#"{}
[[notifiers]]
type = "ntfy"
url = "https://ntfy.sh"
topic = "a"
[[notifiers]]
type = "discord"
url = "https://discord.com/api/webhooks/1/x"
[[notifiers]]
type = "slack"
url = "https://hooks.slack.com/services/x"
[[notifiers]]
type = "pushover"
token = "t"
user = "u"
"#,
            crate::config::tests::base_with_real_hash()
        ))
        .unwrap();
        let names: Vec<String> = build_notifiers(&cfg, &reqwest::Client::new())
            .iter()
            .map(|n| n.name().to_string())
            .collect();
        assert_eq!(names, ["ntfy", "discord", "slack", "pushover"]);
    }
}
