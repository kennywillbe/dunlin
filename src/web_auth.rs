//! Session cookies, login rate limiting, CSRF and client IP resolution.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;

use axum::http::header::{ORIGIN, REFERER};
use axum::http::HeaderMap;
use axum_extra::extract::cookie::{Cookie, SameSite};

pub const SESSION_COOKIE: &str = "dunlin_session";
pub const SESSION_TTL_SECS: i64 = 30 * 86_400;
const MAX_FAILURES: usize = 5;
const WINDOW_SECS: i64 = 15 * 60;
/// Map size at which expired entries of every address are dropped.
const SWEEP_AT: usize = 1024;

/// Per-IP login failure tracking. Deliberately per-IP: a single attacker can
/// only lock out their own address, never everyone.
#[derive(Default)]
pub struct LoginLimiter {
    failures: Mutex<HashMap<String, Vec<i64>>>,
}

impl LoginLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read-only for addresses without failures, so a flood of logins from
    /// many addresses does not leave an entry behind for each of them.
    pub fn is_blocked(&self, ip: &str, now: i64) -> bool {
        let mut map = self.failures.lock().unwrap();
        let Some(entry) = map.get_mut(ip) else {
            return false;
        };
        entry.retain(|t| now - *t < WINDOW_SECS);
        let blocked = entry.len() >= MAX_FAILURES;
        if entry.is_empty() {
            map.remove(ip);
        }
        blocked
    }

    pub fn record_failure(&self, ip: &str, now: i64) {
        let mut map = self.failures.lock().unwrap();
        // Entries are otherwise only trimmed when their own address returns;
        // sweep them all once the map gets large.
        if map.len() >= SWEEP_AT {
            map.retain(|_, times| {
                times.retain(|t| now - *t < WINDOW_SECS);
                !times.is_empty()
            });
        }
        let entry = map.entry(ip.to_string()).or_default();
        entry.retain(|t| now - *t < WINDOW_SECS);
        entry.push(now);
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.failures.lock().unwrap().len()
    }

    pub fn record_success(&self, ip: &str) {
        self.failures.lock().unwrap().remove(ip);
    }
}

/// Client IP: the socket by default, proxy headers only when explicitly trusted.
///
/// A proxy appends the address it saw to `X-Forwarded-For`, after whatever the
/// client sent, so only the last entry is trustworthy. Taking the first one let
/// a client pick a fresh address per request and never hit the login limit.
/// This assumes one proxy in front of dunlin, as the config documents.
pub fn client_ip(headers: &HeaderMap, socket: Option<SocketAddr>, trusted_proxy: bool) -> String {
    if trusted_proxy {
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(last) = xff.rsplit(',').next() {
                let last = last.trim();
                if !last.is_empty() {
                    return last.to_string();
                }
            }
        }
        if let Some(real) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
            let real = real.trim();
            if !real.is_empty() {
                return real.to_string();
            }
        }
    }
    socket
        .map(|s| s.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// SameSite plus an Origin/Referer host check on every state-changing request.
pub fn csrf_ok(headers: &HeaderMap) -> bool {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok());
    let Some(host) = host else { return false };
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();

    let candidate = headers
        .get(ORIGIN)
        .or_else(|| headers.get(REFERER))
        .and_then(|v| v.to_str().ok());
    let Some(candidate) = candidate else {
        return false;
    };
    match url::Url::parse(candidate) {
        Ok(u) => u
            .host_str()
            .map(|h| h.eq_ignore_ascii_case(&host))
            .unwrap_or(false),
        Err(_) => false,
    }
}

pub fn session_cookie(token: &str, secure: bool) -> Cookie<'static> {
    let mut c = Cookie::new(SESSION_COOKIE, token.to_string());
    c.set_http_only(true);
    c.set_same_site(SameSite::Lax);
    c.set_path("/");
    c.set_max_age(time::Duration::seconds(SESSION_TTL_SECS));
    c.set_secure(secure);
    c
}

pub fn clear_session_cookie() -> Cookie<'static> {
    let mut c = Cookie::new(SESSION_COOKIE, "");
    c.set_http_only(true);
    c.set_same_site(SameSite::Lax);
    c.set_path("/");
    c.set_max_age(time::Duration::seconds(0));
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn rate_limit_blocks_after_five_failures_and_expires() {
        let l = LoginLimiter::new();
        for i in 0..4 {
            assert!(!l.is_blocked("1.2.3.4", i));
            l.record_failure("1.2.3.4", i);
        }
        assert!(!l.is_blocked("1.2.3.4", 4));
        l.record_failure("1.2.3.4", 4);
        assert!(l.is_blocked("1.2.3.4", 5));
        // other IPs unaffected
        assert!(!l.is_blocked("5.6.7.8", 5));
        // window slides
        assert!(!l.is_blocked("1.2.3.4", 5 + WINDOW_SECS));
        // success clears
        l.record_failure("1.2.3.4", 1000);
        l.record_success("1.2.3.4");
        assert!(!l.is_blocked("1.2.3.4", 1000));
    }

    #[test]
    fn limiter_forgets_addresses_whose_failures_expired() {
        let l = LoginLimiter::new();
        // Checking alone keeps nothing.
        assert!(!l.is_blocked("1.1.1.1", 0));
        assert_eq!(l.tracked(), 0);

        for i in 0..SWEEP_AT {
            l.record_failure(&format!("10.0.{}.{}", i / 256, i % 256), 0);
        }
        assert_eq!(l.tracked(), SWEEP_AT);
        // Past the window, the next failure sweeps the stale ones.
        l.record_failure("9.9.9.9", WINDOW_SECS + 1);
        assert_eq!(l.tracked(), 1);
    }

    #[test]
    fn client_ip_respects_trusted_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("9.9.9.9, 1.1.1.1"),
        );
        let socket: Option<SocketAddr> = Some("127.0.0.1:5000".parse().unwrap());
        assert_eq!(client_ip(&headers, socket, false), "127.0.0.1");
        // 9.9.9.9 is what the client claimed; 1.1.1.1 is what our proxy saw.
        assert_eq!(client_ip(&headers, socket, true), "1.1.1.1");
    }

    #[test]
    fn csrf_requires_matching_origin() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("status.example.org"));
        assert!(!csrf_ok(&headers));
        headers.insert(
            "origin",
            HeaderValue::from_static("http://status.example.org"),
        );
        assert!(csrf_ok(&headers));
        headers.insert(
            "origin",
            HeaderValue::from_static("http://evil.example.org"),
        );
        assert!(!csrf_ok(&headers));
        headers.remove("origin");
        headers.insert(
            "referer",
            HeaderValue::from_static("https://status.example.org/incidents"),
        );
        assert!(csrf_ok(&headers));
    }
}
