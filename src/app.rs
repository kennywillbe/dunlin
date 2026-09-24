//! Runtime wiring: background loops, hot reload and the HTTP server.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::Watcher;
use tokio::sync::{watch, Mutex};

use crate::alert::AlertEngine;
use crate::collector::{configured_mounts, HostCollector};
use crate::config::{self, CheckConfig, CheckType, Config};
use crate::db::{self, Pool};
use crate::docker::{container_samples, DockerSource};
use crate::models::{CheckResult, Notification, Sample};
use crate::notifier::{build_notifiers, MultiNotifier};
use crate::prober::{self, ProbeOutcome};
use crate::status;
use crate::summary;
use crate::systemd::{configured_units, unit_samples, SystemdSource};
use crate::web::{self, AppState};

/// How long the config directory has to stay quiet before a reload.
///
/// One save is several filesystem events (editors and `sed -i` write a temp
/// file and rename it, `cat >>` truncates then writes), and inotify reports
/// each of them; reloading per event parsed the file up to 8 times per edit.
const RELOAD_QUIET: Duration = Duration::from_millis(300);

/// Actor that keeps the config-reload notification stream alive.
///
/// Besides the config file it watches the theme's logo and custom CSS, so a
/// stylesheet edit shows up without touching the config. Theme files added
/// later in a new directory are picked up on the next config edit, not live.
///
/// Must be called inside a tokio runtime: the debounce runs as a task.
pub fn spawn_config_watcher(path: PathBuf, tx: watch::Sender<Arc<Config>>) -> Result<()> {
    // notify reports absolute paths, so a relative `--config dunlin.toml`
    // would never compare equal without canonicalising first.
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut dirs = vec![parent];
    for file in &tx.borrow().theme_files.paths {
        let file = std::fs::canonicalize(file).unwrap_or_else(|_| file.clone());
        if let Some(dir) = file.parent() {
            if !dirs.iter().any(|d| d == dir) {
                dirs.push(dir.to_path_buf());
            }
        }
    }
    let config_rx = tx.subscribe();
    let (reload_tx, reload_rx) = tokio::sync::mpsc::unbounded_channel();
    let reload_path = path.clone();
    tokio::spawn(coalesce_reloads(reload_rx, RELOAD_QUIET, move || {
        apply_reload(&reload_path, &tx);
    }));

    let (event_tx, event_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("config-watch".to_string())
        .spawn(move || {
            let mut watcher = match notify::recommended_watcher(move |res| {
                let _ = event_tx.send(res);
            }) {
                Ok(w) => w,
                Err(e) => {
                    tracing::error!(error = %e, "cannot create config watcher");
                    return;
                }
            };
            for dir in &dirs {
                if let Err(e) = watcher.watch(dir, notify::RecursiveMode::NonRecursive) {
                    tracing::error!(error = %e, dir = %dir.display(), "cannot watch config directory");
                    return;
                }
            }
            for res in event_rx {
                match res {
                    Ok(event) => {
                        let theme = config_rx.borrow().theme_files.paths.clone();
                        if touches_watched(&event.paths, &path, &theme)
                            && reload_tx.send(()).is_err()
                        {
                            return;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "config watch error"),
                }
            }
        })
        .context("spawning config watcher")?;
    Ok(())
}

/// Whether an event concerns the config file or one of the theme files.
///
/// Watching the directory rather than the file is what keeps an atomic
/// replace (temp file renamed over the config) or a `mv` working: the file's
/// inode changes, but the rename still reports the config path.
fn touches_watched(event_paths: &[PathBuf], config: &Path, theme: &[PathBuf]) -> bool {
    let canon = |p: &PathBuf| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
    let theme: Vec<PathBuf> = theme.iter().map(canon).collect();
    event_paths.iter().any(|p| {
        let p = canon(p);
        p == config || theme.contains(&p)
    })
}

/// Run `reload` once per burst of events, after `quiet` without a new one.
async fn coalesce_reloads(
    mut events: tokio::sync::mpsc::UnboundedReceiver<()>,
    quiet: Duration,
    mut reload: impl FnMut(),
) {
    while events.recv().await.is_some() {
        // A closed channel mid-burst still gets its reload, so the last
        // edit before shutdown of the watcher is not silently dropped.
        while let Ok(Some(())) = tokio::time::timeout(quiet, events.recv()).await {}
        reload();
    }
}

/// Reload a file into the running config, keeping the old one when invalid.
pub fn apply_reload(path: &Path, tx: &watch::Sender<Arc<Config>>) {
    match config::load(path) {
        Ok(new) => {
            tracing::info!(path = %path.display(), "configuration reloaded");
            let pending = tx.borrow().restart_only_changes(&new);
            if !pending.is_empty() {
                tracing::warn!(
                    keys = %pending.join(", "),
                    "these settings changed but only take effect after a restart"
                );
            }
            tx.send_replace(Arc::new(new));
        }
        Err(e) => {
            tracing::error!(error = %e, "configuration reload failed; keeping the running config");
        }
    }
}

/// Start every background loop and the HTTP server.
pub async fn run(config_path: PathBuf) -> Result<()> {
    let cfg = Arc::new(config::load(&config_path).context("loading configuration")?);
    let pool = db::connect(&cfg.sqlite_path()).await?;
    let http = prober::http_client()?;
    let notifiers = Arc::new(MultiNotifier::new(build_notifiers(&cfg, &http)));

    let mut engine = AlertEngine::new(notifiers.clone(), cfg.public_url.clone());
    engine.set_subscriptions(cfg.clone());
    engine.bootstrap(&pool, &cfg).await?;
    let engine = Arc::new(Mutex::new(engine));

    let (state, config_tx) = AppState::new(pool.clone(), cfg.clone());
    spawn_config_watcher(config_path, config_tx.clone())?;
    let config_rx = state.config.clone();

    tokio::spawn(notifier_reload_loop(
        config_rx.clone(),
        notifiers.clone(),
        http.clone(),
        cfg.notifiers.clone(),
    ));
    tokio::spawn(collector_loop(pool.clone(), config_rx.clone()));
    tokio::spawn(prober_loop(
        pool.clone(),
        config_rx.clone(),
        engine.clone(),
        http.clone(),
    ));
    tokio::spawn(rollup_loop(pool.clone(), config_rx.clone()));
    state
        .channels
        .replace(crate::subscriptions::dispatch::build_channels(&cfg));
    tokio::spawn(crate::subscriptions::dispatch::dispatcher_loop(
        pool.clone(),
        config_rx.clone(),
        state.channels.clone(),
    ));
    tokio::spawn(crate::subscriptions::dispatch::channel_reload_loop(
        config_rx.clone(),
        state.channels.clone(),
        cfg.subscriptions.clone(),
    ));
    tokio::spawn(summary_loop(
        pool.clone(),
        config_rx.clone(),
        notifiers.clone(),
    ));

    let addr = cfg.listen_addr()?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "dunlin listening");
    axum::serve(
        listener,
        web::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .context("http server")?;
    Ok(())
}

/// Rebuild the notification channels when `[[notifiers]]` changes, so the
/// alert engine and the daily summary send to the reloaded ones. `built_from`
/// is the list `notifiers` was made from; taking it from the receiver instead
/// would miss a reload that lands before this task first runs.
pub async fn notifier_reload_loop(
    mut config_rx: watch::Receiver<Arc<Config>>,
    notifiers: Arc<MultiNotifier>,
    http: reqwest::Client,
    built_from: Vec<crate::config::NotifierConfig>,
) {
    let mut current = built_from;
    while config_rx.changed().await.is_ok() {
        let cfg = config_rx.borrow_and_update().clone();
        if cfg.notifiers != current {
            notifiers.replace(build_notifiers(&cfg, &http));
            current = cfg.notifiers.clone();
            tracing::info!(channels = current.len(), "notifiers reloaded");
        }
    }
}

fn ensure_docker(cfg: &Config, slot: &mut Option<Arc<dyn DockerSource>>) {
    if !cfg.docker.enabled {
        *slot = None;
        return;
    }
    if slot.is_none() {
        match crate::docker::BollardDocker::connect(cfg.docker.socket.as_deref()) {
            Ok(d) => *slot = Some(Arc::new(d)),
            Err(e) => tracing::warn!(error = %e, "docker unavailable"),
        }
    }
}

#[cfg(target_os = "linux")]
async fn ensure_systemd(cfg: &Config, slot: &mut Option<Arc<dyn SystemdSource>>) {
    if !cfg.systemd.enabled {
        *slot = None;
        return;
    }
    if slot.is_none() {
        match crate::systemd::ZbusSystemd::connect().await {
            Ok(s) => *slot = Some(Arc::new(s)),
            Err(e) => tracing::warn!(error = %e, "systemd D-Bus unavailable"),
        }
    }
}

#[cfg(not(target_os = "linux"))]
async fn ensure_systemd(_cfg: &Config, slot: &mut Option<Arc<dyn SystemdSource>>) {
    *slot = None;
}

pub async fn collector_loop(pool: Pool, config_rx: watch::Receiver<Arc<Config>>) {
    let mut host = HostCollector::new();
    let mut docker: Option<Arc<dyn DockerSource>> = None;
    let mut systemd: Option<Arc<dyn SystemdSource>> = None;
    loop {
        let cfg = config_rx.borrow().clone();
        let now = crate::now_ts();
        ensure_docker(&cfg, &mut docker);
        ensure_systemd(&cfg, &mut systemd).await;

        let mounts = configured_mounts(&cfg);
        match host.collect(&cfg.proc_root, &mounts, now) {
            Ok(samples) => {
                if let Err(e) = db::insert_samples(&pool, &samples).await {
                    tracing::warn!(error = %e, "storing host samples");
                }
            }
            Err(e) => tracing::warn!(error = %e, "host collection failed"),
        }

        if let Some(d) = &docker {
            match d.containers().await {
                Ok(containers) => {
                    let samples = container_samples(&containers, now);
                    let _ = db::insert_samples(&pool, &samples).await;
                }
                Err(e) => tracing::warn!(error = %e, "docker collection failed"),
            }
        }

        let units = configured_units(&cfg);
        if let Some(s) = &systemd {
            if !units.is_empty() {
                match s.unit_states(&units).await {
                    Ok(states) => {
                        let samples = unit_samples(&states, now);
                        let _ = db::insert_samples(&pool, &samples).await;
                    }
                    Err(e) => tracing::warn!(error = %e, "systemd collection failed"),
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

struct ProbeEnv<'a> {
    pool: &'a Pool,
    http: &'a reqwest::Client,
    containers: Option<&'a [crate::docker::ContainerStat]>,
    units: Option<&'a [crate::systemd::UnitStat]>,
    /// When this process first ran each check, by id.
    first_run: &'a std::collections::HashMap<String, i64>,
    now: i64,
}

/// Run one check. `None` means there was nothing to judge it by (the Docker
/// or systemd source is off or did not answer), so no result is recorded.
async fn evaluate(check: &CheckConfig, env: &ProbeEnv<'_>) -> Option<ProbeOutcome> {
    let latest = |metric: &'static str, key: String| async move {
        db::latest_sample(env.pool, "host", metric, &key)
            .await
            .ok()
            .flatten()
    };
    Some(match check.kind {
        CheckType::Http => prober::probe_http(env.http, check).await,
        CheckType::Tcp => prober::probe_tcp(check).await,
        CheckType::Disk => {
            let key = check.mount.clone().unwrap_or_default();
            prober::probe_resource(check, latest("disk_used_pct", key).await)
        }
        CheckType::Ram => prober::probe_resource(check, latest("mem_pct", String::new()).await),
        CheckType::Swap => prober::probe_resource(check, latest("swap_pct", String::new()).await),
        CheckType::Load => prober::probe_resource(check, latest("load1", String::new()).await),
        CheckType::Docker => prober::probe_docker(check, env.containers?),
        CheckType::Systemd => prober::probe_systemd(check, env.units?),
        CheckType::Heartbeat => {
            let last = db::last_ping(env.pool, &check.id).await.ok().flatten();
            let since = env.first_run.get(&check.id).copied().unwrap_or(env.now);
            prober::probe_heartbeat(check, last, since, env.now)
        }
    })
}

fn check_samples(check: &CheckConfig, outcome: &ProbeOutcome, now: i64) -> Vec<Sample> {
    let mut out = vec![Sample {
        ts: now,
        scope: "check".to_string(),
        metric: "up".to_string(),
        key: check.id.clone(),
        value: if outcome.ok { 1.0 } else { 0.0 },
    }];
    if let Some(latency) = outcome.latency_ms {
        out.push(Sample {
            ts: now,
            scope: "check".to_string(),
            metric: "latency_ms".to_string(),
            key: check.id.clone(),
            value: latency,
        });
    }
    out
}

pub async fn prober_loop(
    pool: Pool,
    config_rx: watch::Receiver<Arc<Config>>,
    engine: Arc<Mutex<AlertEngine>>,
    http: reqwest::Client,
) {
    let mut last: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut first_run: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut docker: Option<Arc<dyn DockerSource>> = None;
    let mut systemd: Option<Arc<dyn SystemdSource>> = None;
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    // `run` bootstraps the engine against the startup config, so only later
    // swaps need reconciling.
    let mut applied = config_rx.borrow().clone();
    loop {
        ticker.tick().await;
        let cfg = config_rx.borrow().clone();
        let now = crate::now_ts();
        if !Arc::ptr_eq(&cfg, &applied) {
            // Nothing probes a removed check again, so its open incident and
            // machine have to be cleared here or they stay open for good.
            // On failure `applied` stays put so the next tick retries.
            match engine
                .lock()
                .await
                .forget_unmonitored(&pool, &cfg, Some(&applied), now)
                .await
            {
                Ok(_) => {
                    forget_removed_checks(&cfg, &mut last, &mut first_run);
                    applied = cfg.clone();
                }
                Err(e) => tracing::warn!(error = %e, "clearing incidents of removed checks"),
            }
        }
        ensure_docker(&cfg, &mut docker);
        ensure_systemd(&cfg, &mut systemd).await;

        let due: Vec<CheckConfig> = cfg
            .checks
            .iter()
            .filter(|c| now - last.get(&c.id).copied().unwrap_or(0) >= c.interval.as_secs() as i64)
            .cloned()
            .collect();
        if due.is_empty() {
            continue;
        }
        for c in &due {
            last.insert(c.id.clone(), now);
            first_run.entry(c.id.clone()).or_insert(now);
        }

        let needs_docker = due.iter().any(|c| c.kind == CheckType::Docker) && docker.is_some();
        let containers = if needs_docker {
            match docker.as_ref().unwrap().containers().await {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "docker probe data unavailable");
                    None
                }
            }
        } else {
            None
        };

        let units = configured_units(&cfg);
        let needs_systemd = due.iter().any(|c| c.kind == CheckType::Systemd)
            && systemd.is_some()
            && !units.is_empty();
        let unit_states = if needs_systemd {
            match systemd.as_ref().unwrap().unit_states(&units).await {
                Ok(u) => Some(u),
                Err(e) => {
                    tracing::warn!(error = %e, "systemd probe data unavailable");
                    None
                }
            }
        } else {
            None
        };

        let maintenance = db::active_maintenance(&pool, now).await.unwrap_or_default();

        for check in &due {
            let env = ProbeEnv {
                pool: &pool,
                http: &http,
                containers: containers.as_deref(),
                units: unit_states.as_deref(),
                first_run: &first_run,
                now,
            };
            // A Docker/systemd check whose source is off or did not answer is
            // skipped: reporting it up would hide an outage and could resolve
            // its incident, reporting it down would alert on our own problem.
            let Some(outcome) = evaluate(check, &env).await else {
                tracing::warn!(check = %check.id, "check skipped: no data from its source");
                continue;
            };
            let result = CheckResult {
                ts: now,
                check_id: check.id.clone(),
                ok: outcome.ok,
                degraded: outcome.degraded,
                latency_ms: outcome.latency_ms,
                message: outcome.message.clone(),
            };
            if let Err(e) = db::insert_check_result(&pool, &result).await {
                tracing::warn!(error = %e, "storing check result");
            }
            let _ = db::insert_samples(&pool, &check_samples(check, &outcome, now)).await;

            let component = cfg.incident_component(&check.id);
            let muted = muted(&maintenance, &component, now);
            let mut engine = engine.lock().await;
            engine.set_public_url(cfg.public_url.clone());
            engine.set_subscriptions(cfg.clone());
            if let Err(e) = engine
                .handle(
                    &pool,
                    crate::alert::AlertInput {
                        component: &component,
                        check,
                        ok: outcome.ok,
                        message: outcome.message.as_deref(),
                        now,
                        muted,
                    },
                )
                .await
            {
                tracing::warn!(error = %e, "alert engine");
            }
        }
    }
}

/// Whether a window mutes alerts for `component` at `now`. A planned window
/// is not muting yet: only one that has started covers the component.
fn muted(windows: &[crate::models::Maintenance], component: &str, now: i64) -> bool {
    windows.iter().any(|m| m.covers(component, now))
}

/// Drop per-check timing of checks no longer in `cfg`. A heartbeat removed and
/// added back under the same id would otherwise count its first-ping grace
/// from when it was first seen, long ago, and fail at once.
fn forget_removed_checks(
    cfg: &Config,
    last: &mut std::collections::HashMap<String, i64>,
    first_run: &mut std::collections::HashMap<String, i64>,
) {
    last.retain(|id, _| cfg.check(id).is_some());
    first_run.retain(|id, _| cfg.check(id).is_some());
}

pub async fn rollup_loop(pool: Pool, config_rx: watch::Receiver<Arc<Config>>) {
    loop {
        let cfg = config_rx.borrow().clone();
        let now = crate::now_ts();
        if let Err(e) = crate::rollup::run_once(
            &pool,
            now,
            cfg.retention.raw_days,
            cfg.retention.hourly_days,
        )
        .await
        {
            tracing::warn!(error = %e, "rollup failed");
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

pub async fn summary_loop(
    pool: Pool,
    config_rx: watch::Receiver<Arc<Config>>,
    notifiers: Arc<MultiNotifier>,
) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let cfg = config_rx.borrow().clone();
        let Some(summary_cfg) = cfg.summary.clone() else {
            continue;
        };
        let last_sent = db::get_meta(&pool, "last_summary")
            .await
            .ok()
            .flatten()
            .and_then(|s| chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());
        let now_utc = chrono::Utc::now();
        let Some(date) = summary::should_send(&summary_cfg, cfg.tz(), now_utc, last_sent) else {
            continue;
        };
        let notification = build_summary(&pool, &cfg, crate::now_ts()).await;
        notifiers.send(&notification).await;
        let _ = db::set_meta(&pool, "last_summary", &date.to_string()).await;
        tracing::info!("daily summary sent");
    }
}

async fn build_summary(pool: &Pool, cfg: &Config, now: i64) -> Notification {
    let from = now - 86_400;
    let mut uptimes = Vec::new();
    for comp in &cfg.components {
        let Some(check) = comp.check.as_ref().and_then(|id| cfg.check(id)) else {
            continue;
        };
        let (total, up) = db::uptime_between(pool, &check.id, from, now)
            .await
            .unwrap_or((0, 0));
        uptimes.push(summary::ComponentUptime {
            name: comp.name.clone(),
            up,
            total,
        });
    }
    let mut disks = Vec::new();
    for mount in configured_mounts(cfg) {
        if let Ok(Some(pct)) = db::latest_sample(pool, "host", "disk_used_pct", &mount).await {
            disks.push((mount, pct));
        }
    }
    let mem = db::latest_sample(pool, "host", "mem_pct", "")
        .await
        .ok()
        .flatten();
    let swap = db::latest_sample(pool, "host", "swap_pct", "")
        .await
        .ok()
        .flatten();
    let incidents = db::count_incidents_since(pool, from).await.unwrap_or(0);
    summary::compose(&uptimes, &disks, mem, swap, incidents)
}

/// Overall state helper shared with tests.
pub fn overall_of(states: impl IntoIterator<Item = crate::models::State>) -> crate::models::State {
    status::overall(states)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn invalid_reload_keeps_old_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dunlin.toml");
        let valid = format!(
            "listen = \"127.0.0.1:9999\"\n[web]\npassword_hash = \"{}\"\n",
            crate::auth::hash_password("correcthorsebattery").unwrap()
        );
        std::fs::write(&path, &valid).unwrap();
        let cfg = Arc::new(config::load(&path).unwrap());
        let (tx, rx) = watch::channel(cfg.clone());

        // Invalid new content must not replace the running config.
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "this is not = valid toml").unwrap();
        drop(f);
        apply_reload(&path, &tx);
        assert_eq!(rx.borrow().listen, cfg.listen);

        // Valid new content replaces it.
        let valid2 = format!(
            "listen = \"127.0.0.1:1234\"\n[web]\npassword_hash = \"{}\"\n",
            crate::auth::hash_password("correcthorsebattery").unwrap()
        );
        std::fs::write(&path, valid2).unwrap();
        apply_reload(&path, &tx);
        assert_eq!(rx.borrow().listen, "127.0.0.1:1234");
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_events_reloads_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let quiet = Duration::from_millis(300);
        let reloads = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let counter = reloads.clone();
        let task = tokio::spawn(coalesce_reloads(rx, quiet, move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));

        // What one `sed -i` looked like on inotify: 8 events within 10 ms.
        for _ in 0..8 {
            tx.send(()).unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(quiet / 2).await;
        assert_eq!(reloads.load(Ordering::SeqCst), 0, "still inside the burst");
        tokio::time::sleep(quiet).await;
        assert_eq!(reloads.load(Ordering::SeqCst), 1);

        // A later, separate edit gets its own reload.
        tx.send(()).unwrap();
        tokio::time::sleep(quiet * 2).await;
        assert_eq!(reloads.load(Ordering::SeqCst), 2);

        // Events that keep arriving push the reload back until they stop.
        for _ in 0..5 {
            tx.send(()).unwrap();
            tokio::time::sleep(quiet / 2).await;
        }
        assert_eq!(reloads.load(Ordering::SeqCst), 2);
        tokio::time::sleep(quiet).await;
        assert_eq!(reloads.load(Ordering::SeqCst), 3);

        drop(tx);
        task.await.unwrap();
        assert_eq!(reloads.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn replacing_the_config_file_counts_as_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let config = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("dunlin.toml");
        std::fs::write(&config, "a").unwrap();
        let theme = vec![dir.path().join("x.css")];

        // Editor / `sed -i` style: the temp file is gone after the rename,
        // and the event carries both names.
        let temp = dir.path().join(".dunlin.toml.swp");
        assert!(touches_watched(
            &[temp.clone(), config.clone()],
            &config,
            &theme
        ));
        // `mv other dunlin.toml` from a non-canonical spelling of the dir.
        let spelled = dir.path().join(".").join("dunlin.toml");
        assert!(touches_watched(&[spelled], &config, &theme));
        // Theme file edits count, unrelated files do not.
        std::fs::write(dir.path().join("x.css"), "body{}").unwrap();
        assert!(touches_watched(
            &[dir.path().join("x.css")],
            &config,
            &theme
        ));
        assert!(!touches_watched(&[temp], &config, &theme));
    }

    #[test]
    fn removed_checks_lose_their_timing() {
        let cfg = Config {
            checks: vec![test_check()],
            ..Config::default()
        };
        let timing =
            || std::collections::HashMap::from([("c".to_string(), 1), ("gone".to_string(), 2)]);
        let (mut last, mut first_run) = (timing(), timing());
        forget_removed_checks(&cfg, &mut last, &mut first_run);
        assert_eq!(last.keys().collect::<Vec<_>>(), ["c"]);
        assert_eq!(first_run.keys().collect::<Vec<_>>(), ["c"]);
    }

    #[tokio::test]
    async fn reloaded_notifiers_replace_the_running_ones() {
        let (tx, rx) = watch::channel(Arc::new(Config::default()));
        let notifiers = Arc::new(MultiNotifier::new(Vec::new()));
        let http = prober::http_client().unwrap();
        tokio::spawn(notifier_reload_loop(
            rx,
            notifiers.clone(),
            http,
            Vec::new(),
        ));

        let cfg = Config {
            notifiers: vec![crate::config::NotifierConfig::Webhook {
                url: "http://127.0.0.1:9/hook".into(),
                headers: Default::default(),
            }],
            ..Config::default()
        };
        tx.send_replace(Arc::new(cfg));
        tokio::time::timeout(Duration::from_secs(5), async {
            while notifiers.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("notifiers were not rebuilt");
        assert_eq!(notifiers.len(), 1);
    }

    #[test]
    fn restart_only_keys_are_named() {
        let old = Config::default();
        let mut new = Config {
            listen: "127.0.0.1:9090".into(),
            ..Config::default()
        };
        new.docker.socket = Some("/run/docker.sock".into());
        assert_eq!(
            old.restart_only_changes(&new),
            vec!["listen", "docker.socket"]
        );
        assert!(old.restart_only_changes(&old.clone()).is_empty());
    }

    #[tokio::test]
    async fn source_checks_without_data_are_skipped_not_passed() {
        let pool = crate::db::connect_memory().await.unwrap();
        let http = prober::http_client().unwrap();
        let first_run = Default::default();
        let env = ProbeEnv {
            pool: &pool,
            http: &http,
            containers: None,
            units: None,
            first_run: &first_run,
            now: 0,
        };
        let docker = CheckConfig {
            kind: CheckType::Docker,
            container: Some("api".into()),
            ..test_check()
        };
        let systemd = CheckConfig {
            kind: CheckType::Systemd,
            unit: Some("nginx.service".into()),
            ..test_check()
        };
        assert_eq!(evaluate(&docker, &env).await, None);
        assert_eq!(evaluate(&systemd, &env).await, None);

        // With data, the same check is judged.
        let env = ProbeEnv {
            containers: Some(&[]),
            ..env
        };
        let out = evaluate(&docker, &env).await.unwrap();
        assert!(!out.ok, "{out:?}");
    }

    #[tokio::test]
    async fn planned_maintenance_mutes_only_once_started() {
        let pool = crate::db::connect_memory().await.unwrap();
        let now = 1_000_000;
        db::create_maintenance(&pool, "web", "", now + 600, now + 1200, now)
            .await
            .unwrap();
        // The probe loop fetches windows this way before every round.
        let at = |t: i64| {
            let pool = pool.clone();
            async move { db::active_maintenance(&pool, t).await.unwrap() }
        };
        assert!(!muted(&at(now).await, "web", now));
        assert!(!muted(&at(now + 599).await, "web", now + 599));
        assert!(muted(&at(now + 600).await, "web", now + 600));
        assert!(!muted(&at(now + 600).await, "db", now + 600));
        assert!(!muted(&at(now + 1200).await, "web", now + 1200));
    }

    #[tokio::test]
    async fn check_samples_shape() {
        let check = test_check();
        let outcome = ProbeOutcome::up().with_latency(12.0);
        let samples = check_samples(&check, &outcome, 5);
        assert!(samples.iter().any(|s| s.metric == "up" && s.value == 1.0));
        assert!(samples
            .iter()
            .any(|s| s.metric == "latency_ms" && s.value == 12.0));
    }

    fn test_check() -> CheckConfig {
        CheckConfig {
            id: "c".into(),
            name: "c".into(),
            kind: CheckType::Tcp,
            interval: Duration::from_secs(60),
            timeout: None,
            failures_to_open: 1,
            successes_to_resolve: 1,
            reminder_interval: Duration::from_secs(60),
            url: None,
            expected_status: None,
            latency_degraded: None,
            body_contains: None,
            cert_days_warn: None,
            host: None,
            port: None,
            unit: None,
            expected_active: None,
            container: None,
            restart_limit: None,
            mount: None,
            warn: None,
            critical: None,
            period: None,
            grace: None,
            token: None,
        }
    }
}
