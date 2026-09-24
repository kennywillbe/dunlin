//! The email channel: plain-text mail over SMTP.

use std::str::FromStr;

use anyhow::{Context, Result};
use async_trait::async_trait;
use lettre::message::header::{ContentType, Header, HeaderName, HeaderValue};
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Message as Mail, Tokio1Executor};

use super::dispatch::{Delivery, Message, Pacing, PermanentFailure, SubscriberChannel};
use super::{IncidentEvent, MaintenanceEvent, SubscriberEvent};
use crate::config::{EmailConfig, SmtpTls};
use crate::models::{IncidentState, State};

/// Mail per second, kept modest: providers throttle or flag bursts from
/// one sender, and an outage can queue a message for every subscriber.
const PACE: std::time::Duration = std::time::Duration::from_millis(200);

pub struct EmailChannel {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl EmailChannel {
    pub fn new(cfg: &EmailConfig) -> Result<Self> {
        let builder = match cfg.tls {
            SmtpTls::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host),
            SmtpTls::Implicit => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host),
        }
        .with_context(|| format!("smtp server {}", cfg.host))?;
        let mut builder = builder
            .port(cfg.port())
            .timeout(Some(crate::notifier::SEND_TIMEOUT));
        if let (Some(user), Some(pass)) = (&cfg.username, &cfg.password) {
            builder = builder.credentials(Credentials::new(user.clone(), pass.clone()));
        }
        Ok(Self::with_transport(builder.build(), cfg.from.parse()?))
    }

    /// Any transport; tests use an unencrypted one against a local server.
    pub fn with_transport(transport: AsyncSmtpTransport<Tokio1Executor>, from: Mailbox) -> Self {
        Self { transport, from }
    }
}

/// `List-Unsubscribe` (RFC 2369): mail clients show their own unsubscribe
/// button for it.
#[derive(Clone)]
struct ListUnsubscribe(String);

impl Header for ListUnsubscribe {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii_str("List-Unsubscribe")
    }

    fn parse(s: &str) -> std::result::Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self(s.trim_matches(['<', '>']).to_string()))
    }

    fn display(&self) -> HeaderValue {
        HeaderValue::new(Self::name(), format!("<{}>", self.0))
    }
}

/// `List-Unsubscribe-Post` (RFC 8058): the button unsubscribes with one
/// POST instead of opening a page. Gmail and Yahoo expect it from bulk
/// senders.
#[derive(Clone)]
struct ListUnsubscribePost;

impl Header for ListUnsubscribePost {
    fn name() -> HeaderName {
        HeaderName::new_from_ascii_str("List-Unsubscribe-Post")
    }

    fn parse(_: &str) -> std::result::Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self)
    }

    fn display(&self) -> HeaderValue {
        HeaderValue::new(Self::name(), "List-Unsubscribe=One-Click".to_string())
    }
}

fn when(ts: i64, tz: chrono_tz::Tz) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| {
            d.with_timezone(&tz)
                .format("%a %-d %b %Y, %H:%M")
                .to_string()
        })
        .map_or_else(String::new, |t| format!("{t} ({})", tz.name()))
}

fn impact_label(name: &str) -> &'static str {
    State::from_name(name).map_or("Unknown impact", State::label)
}

fn state_label(name: &str) -> &'static str {
    IncidentState::from_name(name).map_or("Update", IncidentState::label)
}

fn incident_body(e: &IncidentEvent) -> String {
    let mut body = format!(
        "{}\n{} · {} · {}\n\n{}\n",
        e.title,
        e.component_name,
        impact_label(&e.impact),
        state_label(&e.state),
        e.message.trim()
    );
    if let Some(url) = &e.url {
        body.push_str(&format!("\nDetails: {url}\n"));
    }
    body
}

fn maintenance_body(e: &MaintenanceEvent, tz: chrono_tz::Tz, headline: &str) -> String {
    let mut body = format!(
        "{headline}\nFrom {}\nto   {}\n",
        when(e.starts_at, tz),
        when(e.ends_at, tz)
    );
    if !e.note.trim().is_empty() {
        body.push_str(&format!("\n{}\n", e.note.trim()));
    }
    if let Some(url) = &e.url {
        body.push_str(&format!("\nStatus page: {url}\n"));
    }
    body
}

/// Subject and body of a message, without the unsubscribe footer.
pub fn compose(d: &Delivery) -> (String, String) {
    let site = &d.site_title;
    match &d.message {
        Message::Confirm { confirm_url, .. } => (
            format!("Confirm your subscription to {site}"),
            format!(
                "Someone, hopefully you, asked for status updates from {site} at this address.\n\n\
                 Confirm here: {confirm_url}\n\n\
                 The link works for 24 hours. If this was not you, ignore this mail and nothing more will be sent.\n"
            ),
        ),
        Message::Event { event, .. } => match event {
            SubscriberEvent::IncidentOpened(e) => (
                format!("[{site}] Incident: {}", e.title),
                incident_body(e),
            ),
            SubscriberEvent::IncidentUpdated(e) => (
                format!("[{site}] Update: {}", e.title),
                incident_body(e),
            ),
            SubscriberEvent::IncidentResolved(e) => (
                format!("[{site}] Resolved: {}", e.title),
                incident_body(e),
            ),
            SubscriberEvent::MaintenanceScheduled(e) => (
                format!("[{site}] Maintenance planned: {}", e.component_name),
                maintenance_body(e, d.timezone, &format!("Maintenance is planned on {}.", e.component_name)),
            ),
            SubscriberEvent::MaintenanceStarted(e) => (
                format!("[{site}] Maintenance started: {}", e.component_name),
                maintenance_body(e, d.timezone, &format!("Maintenance on {} has started.", e.component_name)),
            ),
            SubscriberEvent::MaintenanceCompleted(e) => (
                format!("[{site}] Maintenance completed: {}", e.component_name),
                maintenance_body(e, d.timezone, &format!("Maintenance on {} is complete.", e.component_name)),
            ),
            SubscriberEvent::MaintenanceCancelled(e) => (
                format!("[{site}] Maintenance cancelled: {}", e.component_name),
                maintenance_body(
                    e,
                    d.timezone,
                    &format!("The maintenance planned on {} will not happen.", e.component_name),
                ),
            ),
        },
    }
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

/// Mailbox errors that mean this address will never take mail: no such
/// user (550, 551) or a mailbox name the server refuses (553). Other 5xx
/// replies, such as 535 for a wrong SMTP password, are the sender's
/// problem and are retried, so fixing the config still delivers.
fn is_dead_address(e: &lettre::transport::smtp::Error) -> bool {
    e.status()
        .is_some_and(|c| matches!(u16::from(c), 550 | 551 | 553))
}

#[async_trait]
impl SubscriberChannel for EmailChannel {
    fn name(&self) -> &'static str {
        "email"
    }

    fn label(&self) -> &str {
        "Email"
    }

    fn address_hint(&self) -> &str {
        "your email address"
    }

    fn normalize_address(&self, input: &str) -> std::result::Result<String, String> {
        normalize(input)
    }

    fn pacing(&self) -> Pacing {
        Pacing {
            global: PACE,
            per_address: std::time::Duration::ZERO,
        }
    }

    async fn send(&self, d: &Delivery) -> Result<()> {
        let (subject, mut body) = compose(d);
        let unsubscribe = unsubscribe_url(d).to_string();
        body.push_str(&format!(
            "\n-- \nYou get this because you subscribed to updates from {}.\nUnsubscribe: {unsubscribe}\n",
            d.site_title
        ));
        let to: Mailbox = Mailbox::new(None, d.address.parse().context("recipient")?);
        let mail = Mail::builder()
            .from(self.from.clone())
            .to(to)
            .subject(subject)
            .header(ListUnsubscribe(unsubscribe))
            .header(ListUnsubscribePost)
            .header(ContentType::TEXT_PLAIN)
            .body(body)
            .context("building mail")?;
        match self.transport.send(mail).await {
            Ok(_) => Ok(()),
            Err(e) if is_dead_address(&e) => Err(PermanentFailure(e.to_string()).into()),
            Err(e) => Err(anyhow::Error::new(e).context("smtp")),
        }
    }
}

/// Trim, lowercase the domain (the local part is the server's business and
/// may be case sensitive), and check the syntax.
pub fn normalize(input: &str) -> std::result::Result<String, String> {
    const BAD: &str = "That does not look like an email address.";
    let input = input.trim();
    let (local, domain) = input.rsplit_once('@').ok_or_else(|| BAD.to_string())?;
    let address = format!("{local}@{}", domain.to_lowercase());
    if address.len() > 254 || !domain.contains('.') {
        return Err(BAD.to_string());
    }
    Address::from_str(&address).map_err(|_| BAD.to_string())?;
    Ok(address)
}

#[cfg(test)]
mod tests;
