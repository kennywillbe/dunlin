//! Pages for visitors signing up for updates, and the operator's list.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::CookieJar;

use super::{
    error_page, internal, is_logged_in, not_found, read_guard, render, write_guard, AppState,
};
use crate::config::Config;
use crate::subscriptions::{self, ConfirmLink, SignUp};
use crate::templates::*;
use crate::web_auth::{client_ip, csrf_ok};

/// What the visitor sent, kept to fill the form again after an error.
#[derive(Default)]
struct SubscribeForm {
    channel: String,
    address: String,
    all: bool,
    components: Vec<String>,
    maintenance: bool,
    /// The honeypot: people never see it, so anything here came from a bot.
    website: String,
}

impl SubscribeForm {
    /// Parsed by hand because the component checkboxes repeat one key,
    /// which the `Form` extractor cannot collect.
    fn parse(body: &str) -> Self {
        let mut f = Self::default();
        for (k, v) in url::form_urlencoded::parse(body.as_bytes()) {
            match k.as_ref() {
                "channel" => f.channel = v.into_owned(),
                "address" => f.address = v.into_owned(),
                "all" => f.all = true,
                "components" => f.components.push(v.into_owned()),
                "maintenance" => f.maintenance = true,
                "website" => f.website = v.into_owned(),
                _ => {}
            }
        }
        f
    }
}

fn side() -> SideView {
    SideView::plain(
        "Hear about it first.",
        "Get a message when an incident opens, changes or is resolved.",
    )
}

fn form_page(
    state: &AppState,
    cfg: &Config,
    logged_in: bool,
    form: &SubscribeForm,
    error: Option<String>,
) -> Response {
    let channels = state.channels.list();
    let only = channels.len() == 1;
    let page = SubscribeTemplate {
        site: SiteView::new(cfg, logged_in, "subscribe"),
        side: side(),
        channels: channels
            .iter()
            .map(|c| ChannelChoice {
                name: c.name().to_string(),
                label: c.label().to_string(),
                hint: c.address_hint().to_string(),
                checked: only || form.channel == c.name(),
            })
            .collect(),
        components: cfg
            .components
            .iter()
            .map(|c| ComponentChoice {
                id: c.id.clone(),
                name: c.name.clone(),
                checked: form.components.contains(&c.id),
            })
            .collect(),
        address: form.address.clone(),
        all: form.all,
        maintenance: form.maintenance,
        error,
    };
    render(&page)
}

fn notice(
    cfg: &Config,
    heading: &str,
    paragraphs: &[&str],
    action: Option<NoticeAction>,
) -> Response {
    render(&NoticeTemplate {
        site: SiteView::new(cfg, false, "subscribe"),
        side: side(),
        heading: heading.to_string(),
        paragraphs: paragraphs.iter().map(|p| p.to_string()).collect(),
        action,
    })
}

pub(super) async fn subscribe_page(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: axum::http::Uri,
) -> Response {
    let cfg = state.cfg();
    if !cfg.subscriptions.enabled {
        return not_found(State(state), uri).await;
    }
    // A page that cannot be read cannot be subscribed to either.
    if let Some(r) = read_guard(&state, &jar).await {
        return r;
    }
    let logged_in = is_logged_in(&state, &jar).await;
    let form = SubscribeForm {
        all: true,
        ..Default::default()
    };
    form_page(&state, &cfg, logged_in, &form, None)
}

pub(super) async fn subscribe_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    jar: CookieJar,
    uri: axum::http::Uri,
    body: String,
) -> Response {
    let cfg = state.cfg();
    if !cfg.subscriptions.enabled {
        return not_found(State(state), uri).await;
    }
    if let Some(r) = read_guard(&state, &jar).await {
        return r;
    }
    if !csrf_ok(&headers) {
        return (StatusCode::FORBIDDEN, "CSRF check failed").into_response();
    }
    let logged_in = is_logged_in(&state, &jar).await;
    let form = SubscribeForm::parse(&body);
    let ip = client_ip(&headers, Some(addr), cfg.trusted_proxy);
    let now = crate::now_ts();
    if state.subscribe_limiter.is_blocked(&ip, now) {
        let page = form_page(
            &state,
            &cfg,
            logged_in,
            &form,
            Some("Too many sign-ups from your address. Try again in an hour.".to_string()),
        );
        return (StatusCode::TOO_MANY_REQUESTS, page).into_response();
    }
    state.subscribe_limiter.record_failure(&ip, now);

    let done = || {
        notice(
            &cfg,
            "Check for a confirmation",
            &[
                "If that address can receive messages, a confirmation link is on its way.",
                "Nothing else is sent until you open it. The link works for 24 hours.",
            ],
            None,
        )
    };
    // A bot gets the same answer as a person, so it has nothing to learn.
    if !form.website.is_empty() {
        return done();
    }

    let invalid = |msg: &str| {
        (
            StatusCode::BAD_REQUEST,
            form_page(&state, &cfg, logged_in, &form, Some(msg.to_string())),
        )
            .into_response()
    };
    let Some(channel) = state.channels.get(&form.channel) else {
        return invalid("Pick how to get updates.");
    };
    let address = match channel.normalize_address(&form.address) {
        Ok(a) => a,
        Err(e) => return invalid(&e),
    };
    let components: Vec<String> = form
        .components
        .iter()
        .filter(|id| cfg.component(id).is_some())
        .cloned()
        .collect();
    if !form.all && components.is_empty() {
        return invalid("Pick at least one component, or all of them.");
    }
    let req = SignUp {
        channel: channel.name().to_string(),
        address,
        all_components: form.all,
        components,
        maintenance: form.maintenance,
    };
    if let Err(e) = subscriptions::sign_up(&state.pool, &req, now).await {
        return internal(&state, e);
    }
    done()
}

fn bad_link(cfg: &Config) -> Response {
    error_page(
        cfg,
        StatusCode::NOT_FOUND,
        "Link not valid",
        SideView::plain(
            "This link is not valid.",
            "It may have been used already, or the subscription was removed.",
        ),
    )
}

fn expired(cfg: &Config) -> Response {
    (
        StatusCode::GONE,
        notice(
            cfg,
            "Link expired",
            &["Confirmation links work for 24 hours. Subscribe again to get a new one."],
            Some(NoticeAction {
                url: "/subscribe".to_string(),
                label: "Subscribe again".to_string(),
            }),
        ),
    )
        .into_response()
}

// Confirming takes a button press rather than happening on GET: mail
// scanners open every link in a message, and would otherwise confirm
// sign-ups nobody asked for.
pub(super) async fn confirm_page(
    State(state): State<AppState>,
    Path(token): Path<String>,
    uri: axum::http::Uri,
) -> Response {
    let cfg = state.cfg();
    if !cfg.subscriptions.enabled {
        return not_found(State(state), uri).await;
    }
    match subscriptions::check_confirm(&state.pool, &token, crate::now_ts()).await {
        Ok(ConfirmLink::Valid) => notice(
            &cfg,
            "Confirm your subscription",
            &["One more step: confirm, and updates start with the next incident."],
            Some(NoticeAction {
                url: format!("/subscribe/confirm/{token}"),
                label: "Confirm".to_string(),
            }),
        ),
        Ok(ConfirmLink::Expired) => expired(&cfg),
        Ok(ConfirmLink::Invalid) => bad_link(&cfg),
        Err(e) => internal(&state, e),
    }
}

// No CSRF check on the token routes: the token is the authority, and mail
// providers post RFC 8058 one-click unsubscribes without an Origin header.
pub(super) async fn confirm_submit(
    State(state): State<AppState>,
    Path(token): Path<String>,
    uri: axum::http::Uri,
) -> Response {
    let cfg = state.cfg();
    if !cfg.subscriptions.enabled {
        return not_found(State(state), uri).await;
    }
    match subscriptions::confirm(&state.pool, &token, crate::now_ts()).await {
        Ok(ConfirmLink::Valid) => notice(
            &cfg,
            "You are subscribed",
            &["Every message has a link to unsubscribe."],
            None,
        ),
        Ok(ConfirmLink::Expired) => expired(&cfg),
        Ok(ConfirmLink::Invalid) => bad_link(&cfg),
        Err(e) => internal(&state, e),
    }
}

pub(super) async fn unsubscribe_page(
    State(state): State<AppState>,
    Path(token): Path<String>,
    uri: axum::http::Uri,
) -> Response {
    let cfg = state.cfg();
    if !cfg.subscriptions.enabled {
        return not_found(State(state), uri).await;
    }
    match subscriptions::check_unsubscribe(&state.pool, &token).await {
        Ok(true) => notice(
            &cfg,
            "Unsubscribe",
            &["You will get no more updates from this status page."],
            Some(NoticeAction {
                url: format!("/unsubscribe/{token}"),
                label: "Unsubscribe".to_string(),
            }),
        ),
        Ok(false) => bad_link(&cfg),
        Err(e) => internal(&state, e),
    }
}

pub(super) async fn unsubscribe_submit(
    State(state): State<AppState>,
    Path(token): Path<String>,
    uri: axum::http::Uri,
) -> Response {
    let cfg = state.cfg();
    if !cfg.subscriptions.enabled {
        return not_found(State(state), uri).await;
    }
    match subscriptions::unsubscribe(&state.pool, &token).await {
        Ok(true) => notice(
            &cfg,
            "Unsubscribed",
            &["You will get no more updates."],
            None,
        ),
        Ok(false) => bad_link(&cfg),
        Err(e) => internal(&state, e),
    }
}

pub(super) async fn delete_subscriber(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> Response {
    if let Some(r) = write_guard(&state, &headers, &jar).await {
        return r;
    }
    if let Err(e) = subscriptions::delete(&state.pool, id).await {
        return internal(&state, e);
    }
    Redirect::to("/manage").into_response()
}

/// The /manage section: `None` when subscriptions are off and nobody is
/// left subscribed from before.
pub(super) async fn subscribers_view(state: &AppState, cfg: &Config) -> Option<SubscribersView> {
    let list = subscriptions::list(&state.pool).await.unwrap_or_default();
    if !cfg.subscriptions.enabled && list.is_empty() {
        return None;
    }
    let mut counts: Vec<ChannelCount> = Vec::new();
    for s in &list {
        let i = match counts.iter().position(|c| c.channel == s.channel) {
            Some(i) => i,
            None => {
                counts.push(ChannelCount {
                    channel: s.channel.clone(),
                    active: 0,
                    pending: 0,
                    quarantined: 0,
                });
                counts.len() - 1
            }
        };
        match s.status.as_str() {
            "active" => counts[i].active += 1,
            "quarantined" => counts[i].quarantined += 1,
            _ => counts[i].pending += 1,
        }
    }
    counts.sort_by(|a, b| a.channel.cmp(&b.channel));
    let rows = list
        .iter()
        .map(|s| {
            let mut about = if s.all_components {
                "All components".to_string()
            } else {
                s.components
                    .iter()
                    .map(|id| subscriptions::component_label(cfg, id))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            if s.maintenance {
                about.push_str(" · maintenance");
            }
            SubscriberRow {
                id: s.id,
                channel: s.channel.clone(),
                masked: subscriptions::mask(&s.channel, &s.address),
                status: s.status.clone(),
                about,
            }
        })
        .collect();
    Some(SubscribersView { counts, rows })
}
