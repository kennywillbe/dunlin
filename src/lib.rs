//! dunlin — self-hosted uptime and server monitoring.

/// Unix timestamp in seconds.
pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

pub mod alert;
pub mod app;
pub mod auth;
pub mod badge;
pub mod collector;
pub mod config;
pub mod days;
pub mod db;
pub mod docker;
pub mod models;
pub mod notifier;
pub mod prober;
pub mod procfs;
pub mod rollup;
pub mod sentence;
pub mod status;
pub mod summary;
pub mod systemd;
pub mod templates;
pub mod theme;
pub mod web;
pub mod web_auth;
