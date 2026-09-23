//! Per-check alert state machine and the engine that turns transitions into
//! incidents and notifications.

use std::collections::HashMap;
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
    base_url: Option<String>,
}

impl AlertEngine {
    pub fn new(notifiers: Arc<MultiNotifier>, base_url: Option<String>) -> Self {
        Self {
            machines: HashMap::new(),
            notifiers,
            base_url,
        }
    }

    pub fn machines(&self) -> &HashMap<String, CheckMachine> {
        &self.machines
    }

    /// Rebuild machines for checks that already have an open incident.
    pub async fn bootstrap(&mut self, pool: &Pool, cfg: &Config) -> Result<()> {
        for component in &cfg.components {
            let Some(check_id) = component.check.as_ref().filter(|c| !c.is_empty()) else {
                continue;
            };
            if let Some(incident) = db::active_incident_for(pool, &component.id).await? {
                self.machines
                    .insert(check_id.clone(), CheckMachine::resumed(incident.id));
            }
        }
        Ok(())
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
                            &format!(
                                "Automatically opened. {}",
                                message.unwrap_or("check failed")
                            ),
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
                    })
                    .await;
                }
            }
        }
        Ok(())
    }

    async fn notify(&self, mut n: Notification) {
        if let Some(base) = &self.base_url {
            if let Some(id) = n.incident_id {
                n.message = format!("{}\n{}#/incidents/{}", n.message, base, id);
            }
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
}
