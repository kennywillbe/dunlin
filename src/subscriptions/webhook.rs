//! The webhook channel: updates posted to a URL the visitor gives, shaped
//! for Slack or Discord when the URL is theirs, as JSON otherwise.
//!
//! The URL comes from anyone on the internet, so the server must not be
//! turned into a way to reach hosts behind it (SSRF). Only https on port 443
//! is accepted, a resolver drops every non-public address before a
//! connection is made, IP literals are checked the same way (they never
//! reach a resolver), and redirects are not followed.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

use super::dispatch::{
    CountedFailure, Delivery, Message, Pacing, PermanentFailure, SubscriberChannel,
};
use super::{IncidentEvent, MaintenanceEvent, SubscriberEvent};
use crate::models::State;
use crate::notifier::{slack_escape, truncate_chars, SEND_TIMEOUT};

pub const MAX_URL_LEN: usize = 2048;

/// Whether an address is reachable on the public internet, and so fine to
/// post to. Everything special-purpose is refused: loopback, private and
/// shared (CGNAT) ranges, link-local (cloud metadata lives there),
/// unspecified, multicast, broadcast and reserved, documentation and
/// benchmarking ranges, and IPv6 forms that carry one of those IPv4
/// addresses inside.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn in_v4(ip: Ipv4Addr, net: [u8; 4], bits: u32) -> bool {
    let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
    u32::from(ip) & mask == u32::from(Ipv4Addr::from(net)) & mask
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    const BLOCKED: &[([u8; 4], u32)] = &[
        ([0, 0, 0, 0], 8),       // "this network", unspecified
        ([10, 0, 0, 0], 8),      // private
        ([100, 64, 0, 0], 10),   // shared address space (CGNAT)
        ([127, 0, 0, 0], 8),     // loopback
        ([169, 254, 0, 0], 16),  // link-local, incl. 169.254.169.254 metadata
        ([172, 16, 0, 0], 12),   // private
        ([192, 0, 0, 0], 24),    // IETF protocol assignments
        ([192, 0, 2, 0], 24),    // documentation (TEST-NET-1)
        ([192, 88, 99, 0], 24),  // 6to4 relay anycast (deprecated)
        ([192, 168, 0, 0], 16),  // private
        ([198, 18, 0, 0], 15),   // benchmarking
        ([198, 51, 100, 0], 24), // documentation (TEST-NET-2)
        ([203, 0, 113, 0], 24),  // documentation (TEST-NET-3)
        ([224, 0, 0, 0], 4),     // multicast
        ([240, 0, 0, 0], 4),     // reserved, incl. 255.255.255.255 broadcast
    ];
    !BLOCKED.iter().any(|(net, bits)| in_v4(ip, *net, *bits))
}

fn in_v6(ip: Ipv6Addr, net: u128, bits: u32) -> bool {
    let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
    u128::from(ip) & mask == net & mask
}

fn embedded_v4(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo))
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // Forms that reach an IPv4 host are judged by that host.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    if in_v6(ip, 0x0064_ff9b_0000_0000_0000_0000_0000_0000, 96) {
        return is_public_v4(embedded_v4(s[6], s[7])); // NAT64
    }
    if s[0] == 0x2002 {
        return is_public_v4(embedded_v4(s[1], s[2])); // 6to4
    }
    // Only global unicast (2000::/3) is public; that already leaves out
    // unspecified, loopback, IPv4-compatible, ULA (fc00::/7), link-local
    // (fe80::/10), site-local and multicast.
    if !in_v6(ip, 0x2000u128 << 112, 3) {
        return false;
    }
    const BLOCKED: &[(u128, u32)] = &[
        (0x2001_0000u128 << 96, 23), // IETF protocol assignments, Teredo, benchmarking
        (0x2001_0db8u128 << 96, 32), // documentation
        (0x3fff_0000u128 << 96, 20), // documentation (RFC 9637)
    ];
    !BLOCKED.iter().any(|(net, bits)| in_v6(ip, *net, *bits))
}

type Lookup = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send>>
        + Send
        + Sync,
>;

/// A resolver that only ever hands out public addresses. A name with none
/// left fails to resolve, so no connection is attempted at all; checking
/// here rather than on the URL also covers a name that changes its
/// answer between sign-up and send.
#[derive(Clone)]
pub struct PublicResolver {
    lookup: Lookup,
}

impl PublicResolver {
    pub fn system() -> Self {
        Self::with_lookup(Arc::new(|host: String| {
            Box::pin(async move {
                Ok(tokio::net::lookup_host((host.as_str(), 0))
                    .await?
                    .map(|a| a.ip())
                    .collect())
            })
        }))
    }

    /// With a stand-in for DNS, so tests can make a name point anywhere.
    pub fn with_lookup(lookup: Lookup) -> Self {
        Self { lookup }
    }
}

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let lookup = self.lookup.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found = lookup(host.clone()).await?;
            let public: Vec<SocketAddr> = found
                .into_iter()
                .filter(|ip| is_public(*ip))
                .map(|ip| SocketAddr::new(ip, 0))
                .collect();
            if public.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

/// The client for subscriber URLs only. The operator's notifiers keep their
/// own client, since their webhooks may rightly point at internal hosts.
pub fn guarded_client(resolver: PublicResolver) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(resolver))
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        // A proxy would resolve the name itself, past the resolver above.
        .no_proxy()
        .timeout(SEND_TIMEOUT)
        .user_agent(concat!("dunlin/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the subscriber webhook client")
}

const BAD_URL: &str =
    "That is not a webhook URL updates can be sent to. It must start with https://.";

/// Check a subscriber URL. `strict` is off only in tests, which post to
/// local mock servers over http.
pub fn check_url(input: &str, strict: bool) -> std::result::Result<Url, String> {
    let input = input.trim();
    if input.len() > MAX_URL_LEN {
        return Err(format!(
            "The URL is too long; at most {MAX_URL_LEN} characters."
        ));
    }
    let url = Url::parse(input).map_err(|_| BAD_URL.to_string())?;
    let scheme_ok = match url.scheme() {
        "https" => true,
        "http" => !strict,
        _ => false,
    };
    if !scheme_ok || url.host().is_none() {
        return Err(BAD_URL.to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("The URL must not contain a user name or password.".to_string());
    }
    if strict {
        // `port()` is None for https on 443.
        if url.port().is_some() {
            return Err("Only the standard https port (443) is supported.".to_string());
        }
        let literal = match url.host() {
            Some(Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
            Some(Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
            _ => None,
        };
        if literal.is_some_and(|ip| !is_public(ip)) {
            return Err("That address is not on the public internet.".to_string());
        }
    }
    Ok(url)
}

/// Payload shape, picked from the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Slack,
    Discord,
    Json,
}

pub fn detect(url: &Url) -> Format {
    let host = url.host_str().unwrap_or("");
    if host == "hooks.slack.com" {
        Format::Slack
    } else if matches!(
        host,
        "discord.com" | "discordapp.com" | "ptb.discord.com" | "canary.discord.com"
    ) && url.path().starts_with("/api/webhooks/")
    {
        Format::Discord
    } else {
        Format::Json
    }
}

/// The colour a message is drawn in: the incident's impact while it is
/// open, green once resolved or over, blue for maintenance to come.
fn colour(d: &Delivery) -> u32 {
    let state = match &d.message {
        Message::Confirm { .. } => State::Maintenance,
        Message::Event { event, .. } => match event {
            SubscriberEvent::IncidentOpened(e) | SubscriberEvent::IncidentUpdated(e) => {
                State::from_name(&e.impact).unwrap_or(State::MajorOutage)
            }
            SubscriberEvent::IncidentResolved(_)
            | SubscriberEvent::MaintenanceCompleted(_)
            | SubscriberEvent::MaintenanceCancelled(_) => State::Operational,
            SubscriberEvent::MaintenanceScheduled(_) | SubscriberEvent::MaintenanceStarted(_) => {
                State::Maintenance
            }
        },
    };
    state.rgb()
}

/// Where the message's title links to.
fn link(d: &Delivery) -> Option<&str> {
    match &d.message {
        Message::Confirm { confirm_url, .. } => Some(confirm_url),
        Message::Event { event, .. } => match event {
            SubscriberEvent::IncidentOpened(e)
            | SubscriberEvent::IncidentUpdated(e)
            | SubscriberEvent::IncidentResolved(e) => e.url.as_deref(),
            SubscriberEvent::MaintenanceScheduled(e)
            | SubscriberEvent::MaintenanceStarted(e)
            | SubscriberEvent::MaintenanceCompleted(e)
            | SubscriberEvent::MaintenanceCancelled(e) => e.url.as_deref(),
        },
    }
}

fn unsubscribe(d: &Delivery) -> &str {
    match &d.message {
        Message::Confirm {
            unsubscribe_url, ..
        }
        | Message::Event {
            unsubscribe_url, ..
        } => unsubscribe_url,
    }
}

/// Title and text for a chat message: the email wording, so every channel
/// says the same thing, with the unsubscribe link at the end.
fn chat_text(d: &Delivery) -> (String, String) {
    let (title, body) = super::email::compose(d);
    (
        title,
        format!("{}\n\nUnsubscribe: {}", body.trim_end(), unsubscribe(d)),
    )
}

pub fn slack_payload(d: &Delivery, now: i64) -> serde_json::Value {
    let (title, text) = chat_text(d);
    let title = slack_escape(&title);
    let mut attachment = serde_json::json!({
        "color": format!("#{:06x}", colour(d)),
        "title": title,
        "text": slack_escape(&text),
        "ts": now,
    });
    if let Some(link) = link(d) {
        attachment["title_link"] = link.into();
    }
    serde_json::json!({ "text": title, "attachments": [attachment] })
}

pub fn discord_payload(d: &Delivery, now: i64) -> serde_json::Value {
    let (title, text) = chat_text(d);
    let mut embed = serde_json::json!({
        "title": truncate_chars(&title, 256),
        "description": truncate_chars(&text, 4096),
        "color": colour(d),
        "timestamp": chrono::DateTime::from_timestamp(now, 0)
            .unwrap_or_default()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });
    if let Some(link) = link(d) {
        embed["url"] = link.into();
    }
    serde_json::json!({
        "embeds": [embed],
        // Incident text is typed by operators and probes; never a ping.
        "allowed_mentions": { "parse": [] },
    })
}

fn component(id: &str, name: &str) -> serde_json::Value {
    serde_json::json!({ "id": id, "name": name })
}

fn incident_json(e: &IncidentEvent) -> serde_json::Value {
    serde_json::json!({
        "id": e.incident_id,
        "title": e.title,
        "component": component(&e.component, &e.component_name),
        "impact": e.impact,
        "state": e.state,
        "message": e.message,
        "url": e.url,
    })
}

fn maintenance_json(e: &MaintenanceEvent) -> serde_json::Value {
    serde_json::json!({
        "id": e.maintenance_id,
        "component": component(&e.component, &e.component_name),
        "note": e.note,
        "starts_at": e.starts_at,
        "ends_at": e.ends_at,
        "url": e.url,
    })
}

/// The documented JSON body for any other endpoint.
pub fn json_payload(d: &Delivery, now: i64) -> serde_json::Value {
    let mut body = serde_json::json!({
        "meta": { "unsubscribe": unsubscribe(d), "generated_at": now },
        "page": { "title": d.site_title, "url": d.site_url },
    });
    match &d.message {
        Message::Confirm { confirm_url, .. } => {
            body["event"] = "confirm".into();
            body["confirm"] = serde_json::json!({ "url": confirm_url });
        }
        Message::Event { event, .. } => {
            body["event"] = event.kind().into();
            match event {
                SubscriberEvent::IncidentOpened(e)
                | SubscriberEvent::IncidentUpdated(e)
                | SubscriberEvent::IncidentResolved(e) => body["incident"] = incident_json(e),
                SubscriberEvent::MaintenanceScheduled(e)
                | SubscriberEvent::MaintenanceStarted(e)
                | SubscriberEvent::MaintenanceCompleted(e)
                | SubscriberEvent::MaintenanceCancelled(e) => {
                    body["maintenance"] = maintenance_json(e)
                }
            }
        }
    }
    body
}

pub struct WebhookChannel {
    client: reqwest::Client,
    strict: bool,
}

impl WebhookChannel {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: guarded_client(PublicResolver::system())?,
            strict: true,
        })
    }

    /// Plain client, http and local addresses allowed: for tests against a
    /// mock server only. Deliberately not reachable from the config.
    #[cfg(test)]
    pub fn local(client: reqwest::Client) -> Self {
        Self {
            client,
            strict: false,
        }
    }
}

#[async_trait]
impl SubscriberChannel for WebhookChannel {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn label(&self) -> &str {
        "Webhook"
    }

    fn address_hint(&self) -> &str {
        "a Slack, Discord or other https webhook URL"
    }

    fn normalize_address(&self, input: &str) -> std::result::Result<String, String> {
        check_url(input, self.strict).map(|u| u.to_string())
    }

    fn pacing(&self) -> Pacing {
        // Slack and Discord both allow about one message a second per
        // webhook; keeping to it avoids the 429s that would otherwise
        // count against the subscriber.
        Pacing {
            global: Duration::ZERO,
            per_address: Duration::from_secs(1),
        }
    }

    async fn send(&self, d: &Delivery) -> Result<()> {
        // Checked again at send time: IP literals never reach the resolver.
        let url = check_url(&d.address, self.strict)
            .map_err(|e| PermanentFailure(format!("refused URL: {e}")))?;
        let now = crate::now_ts();
        let body = match detect(&url) {
            Format::Slack => slack_payload(d, now),
            Format::Discord => discord_payload(d, now),
            Format::Json => json_payload(d, now),
        };
        let resp = match self.client.post(url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => return Err(CountedFailure(format!("{e:#}")).into()),
        };
        // The body is never read: it is the far side's to fill and could
        // be anything, of any size.
        let status = resp.status();
        drop(resp);
        if status.is_success() {
            Ok(())
        } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Busy, not broken: the outbox backoff waits and it does not
            // count toward quarantine.
            anyhow::bail!("webhook returned {status}")
        } else if matches!(status.as_u16(), 404 | 410) {
            // Slack and Discord answer a deleted webhook this way.
            Err(PermanentFailure(format!("webhook returned {status}")).into())
        } else {
            Err(CountedFailure(format!("webhook returned {status}")).into())
        }
    }
}

#[cfg(test)]
mod tests;
