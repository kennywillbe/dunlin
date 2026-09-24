//! Per-check alert state machine and the engine that turns transitions into
//! incidents and notifications.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;

use crate::config::{CheckConfig, Config};
use crate::db::{self, Pool};
use crate::models::{Health, IncidentState, Notification, State};
use crate::notifier::MultiNotifier;

/// What the state machine wants done after one result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    Nothing,
    Open,
    Resolve(i64),
    Remind(i64),
}

/// Failure/success counters for one check.
#[derive(Debug, Clone)]
pub struct CheckMachine {
    pub state: Health,
    pub failures: u32,
    pub successes: u32,
    pub last_reminder: Option<i64>,
    pub incident_id: Option<i64>,
}

impl Default for CheckMachine {
    fn default() -> Self {
        Self {
            state: Health::Up,
            failures: 0,
            successes: 0,
            last_reminder: None,
            incident_id: None,
        }
    }
}

impl CheckMachine {
    /// Rebuild a machine after a restart from an already-open incident.
    pub fn resumed(incident_id: i64) -> Self {
        Self {
            state: Health::Down,
            incident_id: Some(incident_id),
            ..Self::default()
        }
    }

    pub fn on_result(&mut self, check: &CheckConfig, ok: bool, now: i64) -> Transition {
        if ok {
            self.failures = 0;
            self.successes += 1;
            if self.state == Health::Down && self.successes >= check.successes_to_resolve {
                self.state = Health::Up;
                self.successes = 0;
                self.last_reminder = None;
                if let Some(id) = self.incident_id.take() {
                    return Transition::Resolve(id);
                }
            }
            Transition::Nothing
        } else {
            self.successes = 0;
            self.failures += 1;
            if self.state == Health::Up {
                if self.failures >= check.failures_to_open {
                    self.state = Health::Down;
                    self.last_reminder = Some(now);
                    return Transition::Open;
                }
                Transition::Nothing
            } else {
                let reminder = check.reminder_interval.as_secs() as i64;
                if self
                    .last_reminder
                    .map(|t| now - t >= reminder)
                    .unwrap_or(true)
                {
                    self.last_reminder = Some(now);
                    if let Some(id) = self.incident_id {
                        return Transition::Remind(id);
                    }
                }
                Transition::Nothing
            }
        }
    }
}

/// One probe outcome fed into the engine.
pub struct AlertInput<'a> {
    pub component: &'a str,
    pub check: &'a CheckConfig,
    pub ok: bool,
    pub message: Option<&'a str>,
    pub now: i64,
    /// Suppress notifications (maintenance window).
    pub muted: bool,
}

/// Owns every check's machine and applies transitions to the database.
pub struct AlertEngine {
    machines: HashMap<String, CheckMachine>,
    notifiers: Arc<MultiNotifier>,
    /// `public_url` from the config; notifications link to the incident.
    public_url: Option<String>,
}

impl AlertEngine {
    pub fn new(notifiers: Arc<MultiNotifier>, public_url: Option<String>) -> Self {
        Self {
            machines: HashMap::new(),
            notifiers,
            public_url,
        }
    }

    /// Follow a reloaded `public_url`.
    pub fn set_public_url(&mut self, public_url: Option<String>) {
        self.public_url = public_url;
    }

    pub fn machines(&self) -> &HashMap<String, CheckMachine> {
        &self.machines
    }

    /// Rebuild machines for checks that already have an open incident. Walks
    /// the checks rather than the components: a check no component mirrors
    /// files its incidents under its own id, and skipping it left such an
    /// incident open forever once the check recovered after a restart.
    pub async fn bootstrap(&mut self, pool: &Pool, cfg: &Config) -> Result<()> {
        // Incidents left open by a run whose config had more components than
        // this one would otherwise never resolve, since no check feeds them.
        self.forget_unmonitored(pool, cfg, None, crate::now_ts())
            .await?;
        for check in &cfg.checks {
            let component = cfg.incident_component(&check.id);
            if let Some(incident) = db::active_incident_for(pool, &component).await? {
                self.machines
                    .insert(check.id.clone(), CheckMachine::resumed(incident.id));
            }
        }
        Ok(())
    }

    /// Resolve automatic incidents on components that no check in `cfg`
    /// reports to any more, and drop machines of checks that are gone.
    ///
    /// `previous` is only used to put the component's display name in the
    /// timeline, since the new config no longer knows it. Returns the ids of
    /// the incidents it resolved.
    pub async fn forget_unmonitored(
        &mut self,
        pool: &Pool,
        cfg: &Config,
        previous: Option<&Config>,
        now: i64,
    ) -> Result<Vec<i64>> {
        let monitored: HashSet<String> = cfg
            .checks
            .iter()
            .map(|c| cfg.incident_component(&c.id))
            .chain(
                cfg.components
                    .iter()
                    .filter(|c| c.check.as_deref().is_some_and(|id| !id.is_empty()))
                    .map(|c| c.id.clone()),
            )
            .collect();
        let mut resolved = Vec::new();
        for incident in db::active_incidents(pool).await? {
            // Manual incidents were opened by an operator and are theirs to
            // close; the status page shows them under the raw component id.
            if !incident.auto || monitored.contains(&incident.component) {
                continue;
            }
            let name = previous
                .and_then(|p| p.component(&incident.component))
                .map_or(incident.component.as_str(), |c| c.name.as_str());
            db::resolve_incident(pool, incident.id, now).await?;
            db::add_update(
                pool,
                incident.id,
                now,
                IncidentState::Resolved,
                &format!("Resolved: {name} is no longer monitored."),
                true,
            )
            .await?;
            // No notification: removing the check from the config was a
            // deliberate act, so a "recovered" alert would be false and noise.
            tracing::info!(
                incident = incident.id,
                component = %incident.component,
                "resolved incident for a component that is no longer monitored"
            );
            resolved.push(incident.id);
        }
        // A machine still pointing at one of these incidents would resolve it a
        // second time (and send "recovered") once its check passes again, e.g.
        // when the check stays but moved to another component.
        self.machines.retain(|check_id, m| {
            cfg.check(check_id).is_some() && m.incident_id.is_none_or(|id| !resolved.contains(&id))
        });
        Ok(resolved)
    }

    /// Feed one probe outcome. `muted` suppresses notifications (maintenance)
    /// but still lets incidents open and resolve so history stays consistent.
    pub async fn handle(&mut self, pool: &Pool, input: AlertInput<'_>) -> Result<()> {
        let AlertInput {
            component,
            check,
            ok,
            message,
            now,
            muted,
        } = input;
        // An operator may have resolved this check's incident on /manage. The
        // machine would otherwise keep reminding about a closed incident, never
        // open a new one, and resolve the closed one a second time on recovery.
        // Starting over means a failure that persists opens a fresh incident.
        if let Some(id) = self.machines.get(&check.id).and_then(|m| m.incident_id) {
            let closed = db::incident(pool, id)
                .await?
                .is_none_or(|i| i.resolved_at.is_some());
            if closed {
                self.machines
                    .insert(check.id.clone(), CheckMachine::default());
            }
        }
        let transition = {
            let machine = self.machines.entry(check.id.clone()).or_default();
            machine.on_result(check, ok, now)
        };

        match transition {
            Transition::Nothing => {}
            Transition::Open => {
                let id = match db::active_incident_for(pool, component).await? {
                    Some(existing) => existing.id,
                    None => {
                        let id = db::create_incident(
                            pool,
                            component,
                            &format!("{} is down", check.name),
                            State::MajorOutage,
                            IncidentState::Investigating,
                            true,
                            now,
                        )
                        .await?;
                        db::add_update(
                            pool,
                            id,
                            now,
                            IncidentState::Investigating,
                            // This text leads the status page's detail paragraph, so it
                            // reads as a sentence rather than a raw probe error.
                            &sentence_case(message.unwrap_or("check failed")),
                            true,
                        )
                        .await?;
                        id
                    }
                };
                if let Some(m) = self.machines.get_mut(&check.id) {
                    m.incident_id = Some(id);
                }
                if !muted {
                    self.notify(Notification {
                        event: "down".to_string(),
                        title: format!("{} is down", check.name),
                        message: message.unwrap_or("check failed").to_string(),
                        component: component.to_string(),
                        state: State::MajorOutage,
                        incident_id: Some(id),
                        link: None,
                    })
                    .await;
                }
            }
            Transition::Resolve(id) => {
                db::resolve_incident(pool, id, now).await?;
                db::add_update(
                    pool,
                    id,
                    now,
                    IncidentState::Resolved,
                    "Automatically resolved after recovery.",
                    true,
                )
                .await?;
                if !muted {
                    self.notify(Notification {
                        event: "up".to_string(),
                        title: format!("{} recovered", check.name),
                        message: "The check is passing again.".to_string(),
                        component: component.to_string(),
                        state: State::Operational,
                        incident_id: Some(id),
                        link: None,
                    })
                    .await;
                }
            }
            Transition::Remind(id) => {
                if !muted {
                    self.notify(Notification {
                        event: "reminder".to_string(),
                        title: format!("{} is still down", check.name),
                        message: message.unwrap_or("check still failing").to_string(),
                        component: component.to_string(),
                        state: State::MajorOutage,
                        incident_id: Some(id),
                        link: None,
                    })
                    .await;
                }
            }
        }
        Ok(())
    }

    async fn notify(&self, mut n: Notification) {
        if let (Some(base), Some(id)) = (&self.public_url, n.incident_id) {
            let link = incident_url(base, id);
            n.message = format!("{}\n{link}", n.message);
            n.link = Some(link);
        }
        self.notifiers.send(&n).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CheckType, Config};
    use crate::notifier::RecordingNotifier;
    use std::time::Duration;

    fn check() -> CheckConfig {
        CheckConfig {
            id: "c1".into(),
            name: "Web".into(),
            kind: CheckType::Http,
            interval: Duration::from_secs(60),
            timeout: None,
            failures_to_open: 3,
            successes_to_resolve: 2,
            reminder_interval: Duration::from_secs(100),
            url: Some("http://x/".into()),
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

    #[test]
    fn opens_after_n_and_resolves_after_m() {
        let c = check();
        let mut m = CheckMachine::default();
        assert_eq!(m.on_result(&c, false, 0), Transition::Nothing);
        assert_eq!(m.on_result(&c, false, 1), Transition::Nothing);
        assert_eq!(m.on_result(&c, false, 2), Transition::Open);
        m.incident_id = Some(5);
        assert_eq!(m.on_result(&c, true, 3), Transition::Nothing);
        assert_eq!(m.on_result(&c, true, 4), Transition::Resolve(5));
        assert_eq!(m.state, Health::Up);
    }

    #[test]
    fn reminders_respect_interval() {
        let c = check();
        let mut m = CheckMachine::default();
        m.on_result(&c, false, 0);
        m.on_result(&c, false, 0);
        m.on_result(&c, false, 0); // open
        m.incident_id = Some(9);
        assert_eq!(m.on_result(&c, false, 50), Transition::Nothing);
        assert_eq!(m.on_result(&c, false, 100), Transition::Remind(9));
        assert_eq!(m.on_result(&c, false, 150), Transition::Nothing);
        assert_eq!(m.on_result(&c, false, 200), Transition::Remind(9));
    }

    #[test]
    fn failure_then_success_resets() {
        let c = check();
        let mut m = CheckMachine::default();
        m.on_result(&c, false, 0);
        m.on_result(&c, true, 1); // reset failures
        assert_eq!(m.failures, 0);
        assert_eq!(m.on_result(&c, false, 2), Transition::Nothing);
        assert_eq!(m.on_result(&c, false, 3), Transition::Nothing);
        assert_eq!(m.on_result(&c, false, 4), Transition::Open);
    }

    async fn engine_with(rec: &Arc<RecordingNotifier>) -> AlertEngine {
        let multi = MultiNotifier::new(vec![rec.clone()]);
        AlertEngine::new(Arc::new(multi), None)
    }

    async fn feed(
        engine: &mut AlertEngine,
        pool: &Pool,
        check: &CheckConfig,
        ok: bool,
        message: Option<&str>,
        now: i64,
        muted: bool,
    ) -> Result<()> {
        engine
            .handle(
                pool,
                AlertInput {
                    component: "web",
                    check,
                    ok,
                    message,
                    now,
                    muted,
                },
            )
            .await
    }

    #[tokio::test]
    async fn engine_opens_and_resolves_incidents() {
        let pool = crate::db::connect_memory().await.unwrap();
        let rec = RecordingNotifier::new();
        let mut engine = engine_with(&rec).await;
        let c = check();

        for t in 0..3 {
            feed(&mut engine, &pool, &c, false, Some("boom"), t, false)
                .await
                .unwrap();
        }
        let active = db::active_incidents(&pool).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].component, "web");
        assert_eq!(rec.count(), 1);

        // No duplicate incident on further failures.
        feed(&mut engine, &pool, &c, false, Some("boom"), 10, false)
            .await
            .unwrap();
        assert_eq!(db::active_incidents(&pool).await.unwrap().len(), 1);

        // Recovery resolves.
        feed(&mut engine, &pool, &c, true, None, 20, false)
            .await
            .unwrap();
        feed(&mut engine, &pool, &c, true, None, 21, false)
            .await
            .unwrap();
        assert!(db::active_incidents(&pool).await.unwrap().is_empty());
        assert_eq!(rec.count(), 2);
    }

    #[tokio::test]
    async fn maintenance_mutes_notifications() {
        let pool = crate::db::connect_memory().await.unwrap();
        let rec = RecordingNotifier::new();
        let mut engine = engine_with(&rec).await;
        let c = check();
        for t in 0..3 {
            feed(&mut engine, &pool, &c, false, Some("boom"), t, true)
                .await
                .unwrap();
        }
        // Incident still tracked, but nothing was sent.
        assert_eq!(db::active_incidents(&pool).await.unwrap().len(), 1);
        assert_eq!(rec.count(), 0);
    }

    #[tokio::test]
    async fn bootstrap_resumes_open_incident_and_can_resolve() {
        let pool = crate::db::connect_memory().await.unwrap();
        let id = db::create_incident(
            &pool,
            "web",
            "down",
            State::MajorOutage,
            IncidentState::Investigating,
            true,
            0,
        )
        .await
        .unwrap();
        let rec = RecordingNotifier::new();
        let mut engine = engine_with(&rec).await;

        let cfg = Config {
            components: vec![crate::config::ComponentConfig {
                id: "web".into(),
                name: "Web".into(),
                group: None,
                check: Some("c1".into()),
                description: None,
            }],
            checks: vec![check()],
            ..Config::default()
        };
        engine.bootstrap(&pool, &cfg).await.unwrap();

        let c = check();
        feed(&mut engine, &pool, &c, true, None, 1, false)
            .await
            .unwrap();
        feed(&mut engine, &pool, &c, true, None, 2, false)
            .await
            .unwrap();
        let inc = db::incident(&pool, id).await.unwrap().unwrap();
        assert!(inc.resolved_at.is_some());
        assert_eq!(rec.count(), 1);
    }

    #[tokio::test]
    async fn notifications_link_to_the_incident_under_public_url() {
        let pool = crate::db::connect_memory().await.unwrap();
        let rec = RecordingNotifier::new();
        let multi = MultiNotifier::new(vec![rec.clone()]);
        let mut engine = AlertEngine::new(
            Arc::new(multi),
            Some("https://status.example.org/".to_string()),
        );
        let c = check();
        for t in 0..3 {
            feed(&mut engine, &pool, &c, false, Some("boom"), t, false)
                .await
                .unwrap();
        }
        let id = db::active_incidents(&pool).await.unwrap()[0].id;
        let sent = rec.messages();
        assert_eq!(
            sent[0].message,
            format!("boom\nhttps://status.example.org/incidents/{id}")
        );
        assert_eq!(
            sent[0].link.as_deref(),
            Some(format!("https://status.example.org/incidents/{id}").as_str())
        );
        assert_eq!(sent[0].text(), "boom");
    }

    #[tokio::test]
    async fn operator_resolve_resets_the_machine() {
        let pool = crate::db::connect_memory().await.unwrap();
        let rec = RecordingNotifier::new();
        let mut engine = engine_with(&rec).await;
        let c = check();

        for t in 0..3 {
            feed(&mut engine, &pool, &c, false, Some("boom"), t, false)
                .await
                .unwrap();
        }
        let first = db::active_incidents(&pool).await.unwrap()[0].id;
        assert_eq!(rec.count(), 1);

        // Resolved by hand while the check still fails.
        db::resolve_incident(&pool, first, 5).await.unwrap();

        // No reminder about the closed incident, even past the interval...
        feed(&mut engine, &pool, &c, false, Some("boom"), 200, false)
            .await
            .unwrap();
        assert_eq!(rec.count(), 1);
        assert!(db::active_incidents(&pool).await.unwrap().is_empty());

        // ...and a failure that goes on opens a new incident.
        for t in 201..203 {
            feed(&mut engine, &pool, &c, false, Some("boom"), t, false)
                .await
                .unwrap();
        }
        let active = db::active_incidents(&pool).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_ne!(active[0].id, first);

        // Recovery closes the new one and leaves the old one as it was.
        for t in 300..302 {
            feed(&mut engine, &pool, &c, true, None, t, false)
                .await
                .unwrap();
        }
        assert!(db::active_incidents(&pool).await.unwrap().is_empty());
        let old = db::incident(&pool, first).await.unwrap().unwrap();
        assert_eq!(old.resolved_at, Some(5));
        assert_eq!(db::incident_updates(&pool, first).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn bootstrap_resumes_a_check_no_component_mirrors() {
        let pool = crate::db::connect_memory().await.unwrap();
        // Filed under the check id, as the probe loop does without a component.
        let id = db::create_incident(
            &pool,
            "c1",
            "down",
            State::MajorOutage,
            IncidentState::Investigating,
            true,
            0,
        )
        .await
        .unwrap();
        let rec = RecordingNotifier::new();
        let mut engine = engine_with(&rec).await;
        let cfg = Config {
            checks: vec![check()],
            ..Config::default()
        };
        engine.bootstrap(&pool, &cfg).await.unwrap();

        let c = check();
        for t in 1..=2 {
            engine
                .handle(
                    &pool,
                    AlertInput {
                        component: "c1",
                        check: &c,
                        ok: true,
                        message: None,
                        now: t,
                        muted: false,
                    },
                )
                .await
                .unwrap();
        }
        let inc = db::incident(&pool, id).await.unwrap().unwrap();
        assert!(inc.resolved_at.is_some());
    }

    fn web_config() -> Config {
        Config {
            checks: vec![check()],
            components: vec![crate::config::ComponentConfig {
                id: "web".into(),
                name: "Web site".into(),
                group: None,
                check: Some("c1".into()),
                description: None,
            }],
            ..Config::default()
        }
    }

    async fn manual_incident(pool: &Pool, component: &str) -> i64 {
        db::create_incident(
            pool,
            component,
            "Planned work went wrong",
            State::PartialOutage,
            IncidentState::Identified,
            false,
            0,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn removing_a_component_resolves_its_incident_quietly() {
        let pool = crate::db::connect_memory().await.unwrap();
        let rec = RecordingNotifier::new();
        let mut engine = engine_with(&rec).await;
        let before = web_config();
        let c = check();
        for t in 0..3 {
            feed(&mut engine, &pool, &c, false, Some("boom"), t, false)
                .await
                .unwrap();
        }
        let id = db::active_incident_for(&pool, "web")
            .await
            .unwrap()
            .unwrap()
            .id;
        let manual = manual_incident(&pool, "web").await;
        assert_eq!(rec.count(), 1);
        assert!(engine.machines().contains_key("c1"));

        let after = Config::default();
        let resolved = engine
            .forget_unmonitored(&pool, &after, Some(&before), 50)
            .await
            .unwrap();

        assert_eq!(resolved, vec![id]);
        let inc = db::incident(&pool, id).await.unwrap().unwrap();
        assert_eq!(inc.resolved_at, Some(50));
        assert_eq!(inc.state, IncidentState::Resolved);
        let last = db::incident_updates(&pool, id).await.unwrap();
        let last = last.iter().max_by_key(|u| u.id).unwrap();
        assert_eq!(last.message, "Resolved: Web site is no longer monitored.");
        assert_eq!(last.state, IncidentState::Resolved);
        assert_eq!(rec.count(), 1, "removal must not notify");
        assert!(engine.machines().is_empty());

        // The operator's own incident on the same component stays open.
        let manual = db::incident(&pool, manual).await.unwrap().unwrap();
        assert!(manual.resolved_at.is_none());
        assert_eq!(manual.state, IncidentState::Identified);
    }

    #[tokio::test]
    async fn reload_keeps_incidents_of_components_still_monitored() {
        let pool = crate::db::connect_memory().await.unwrap();
        let rec = RecordingNotifier::new();
        let mut engine = engine_with(&rec).await;
        let cfg = web_config();
        let c = check();
        for t in 0..3 {
            feed(&mut engine, &pool, &c, false, Some("boom"), t, false)
                .await
                .unwrap();
        }
        let resolved = engine
            .forget_unmonitored(&pool, &cfg, Some(&cfg), 50)
            .await
            .unwrap();
        assert!(resolved.is_empty());
        assert_eq!(db::active_incidents(&pool).await.unwrap().len(), 1);
        assert!(engine.machines().contains_key("c1"));
    }

    #[tokio::test]
    async fn bootstrap_resolves_incidents_of_removed_components() {
        let pool = crate::db::connect_memory().await.unwrap();
        let orphan = db::create_incident(
            &pool,
            "gone",
            "Gone is down",
            State::MajorOutage,
            IncidentState::Investigating,
            true,
            0,
        )
        .await
        .unwrap();
        let kept = db::create_incident(
            &pool,
            "web",
            "Web is down",
            State::MajorOutage,
            IncidentState::Investigating,
            true,
            0,
        )
        .await
        .unwrap();
        let manual = manual_incident(&pool, "gone").await;
        let rec = RecordingNotifier::new();
        let mut engine = engine_with(&rec).await;

        engine.bootstrap(&pool, &web_config()).await.unwrap();

        let orphan = db::incident(&pool, orphan).await.unwrap().unwrap();
        assert!(orphan.resolved_at.is_some());
        let updates = db::incident_updates(&pool, orphan.id).await.unwrap();
        // The old config is not available at startup, so the id stands in.
        assert_eq!(
            updates.last().unwrap().message,
            "Resolved: gone is no longer monitored."
        );
        assert!(db::incident(&pool, kept)
            .await
            .unwrap()
            .unwrap()
            .resolved_at
            .is_none());
        assert!(db::incident(&pool, manual)
            .await
            .unwrap()
            .unwrap()
            .resolved_at
            .is_none());
        assert_eq!(engine.machines()["c1"].incident_id, Some(kept));
        assert_eq!(rec.count(), 0);
    }
}

/// Absolute link to an incident page under the configured public URL.
pub fn incident_url(base: &str, id: i64) -> String {
    format!("{}/incidents/{id}", base.trim_end_matches('/'))
}

fn sentence_case(text: &str) -> String {
    let text = text.trim();
    let mut chars = text.chars();
    let mut out: String = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => return String::new(),
    };
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

#[cfg(test)]
mod sentence_case_tests {
    use super::sentence_case;

    #[test]
    fn probe_errors_read_as_sentences() {
        assert_eq!(
            sentence_case("connect failed: Connection refused (os error 61)"),
            "Connect failed: Connection refused (os error 61)."
        );
        assert_eq!(sentence_case("already fine."), "Already fine.");
        assert_eq!(sentence_case("  "), "");
    }
}
