//! Notification channels behind a `Notifier` trait.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::config::{Config, NotifierConfig};
use crate::models::Notification;

#[async_trait]
pub trait Notifier: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, notification: &Notification) -> Result<()>;
}

/// How long one channel may take to accept a notification. Alerts are sent
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
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::State;

    fn sample() -> Notification {
        Notification {
            event: "alert".into(),
            title: "Web is down".into(),
            message: "connection refused".into(),
            component: "web".into(),
            state: State::MajorOutage,
            incident_id: Some(7),
        }
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
}
