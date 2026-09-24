//! Docker container metrics behind a trait so tests can use a fake.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::models::Sample;

/// A point-in-time view of one container.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerStat {
    pub name: String,
    pub running: bool,
    pub health: Option<String>,
    pub cpu_pct: f64,
    pub mem_bytes: u64,
    pub mem_limit_bytes: u64,
    pub restart_count: i64,
}

#[async_trait]
pub trait DockerSource: Send + Sync {
    async fn containers(&self) -> Result<Vec<ContainerStat>>;
}

/// Build chart samples from a container snapshot.
pub fn container_samples(stats: &[ContainerStat], now: i64) -> Vec<Sample> {
    let mut out = Vec::new();
    let mut push = |metric: &str, key: &str, value: f64| {
        out.push(Sample {
            ts: now,
            scope: "container".to_string(),
            metric: metric.to_string(),
            key: key.to_string(),
            value,
        });
    };
    for c in stats {
        push("cpu_pct", &c.name, c.cpu_pct);
        push("mem_bytes", &c.name, c.mem_bytes as f64);
        let mem_pct = if c.mem_limit_bytes > 0 {
            c.mem_bytes as f64 / c.mem_limit_bytes as f64 * 100.0
        } else {
            0.0
        };
        push("mem_pct", &c.name, mem_pct);
        push("restarts", &c.name, c.restart_count as f64);
        push("running", &c.name, if c.running { 1.0 } else { 0.0 });
    }
    out
}

/// Live Docker client.
pub struct BollardDocker {
    docker: bollard::Docker,
    /// Last CPU counters per container id, for when Docker sends no previous
    /// reading of its own.
    last_cpu: Mutex<HashMap<String, CpuCounters>>,
}

impl BollardDocker {
    pub fn connect(socket: Option<&Path>) -> Result<Self> {
        let docker = match socket {
            Some(path) => {
                let path = path.to_string_lossy();
                bollard::Docker::connect_with_unix(&path, 30, bollard::API_DEFAULT_VERSION)
                    .with_context(|| format!("connecting to docker socket {path}"))?
            }
            None => bollard::Docker::connect_with_local_defaults()
                .context("connecting to the local docker daemon")?,
        };
        Ok(Self {
            docker,
            last_cpu: Mutex::new(HashMap::new()),
        })
    }
}

/// Cumulative CPU time of a container and of the whole host, in nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuCounters {
    pub container: u64,
    pub system: u64,
}

/// CPU percent over the interval between two readings, 100 per core.
///
/// A one-shot stats call leaves `precpu_stats` empty, and a percentage taken
/// against zeros is the container's average since the host booted, not its
/// current load. Without a previous reading there is no interval, so 0.
pub fn cpu_percent(cur: CpuCounters, prev: Option<CpuCounters>, cpus: u64) -> f64 {
    let Some(prev) = prev.filter(|p| p.system > 0) else {
        return 0.0;
    };
    let cpu_delta = cur.container.saturating_sub(prev.container) as f64;
    let sys_delta = cur.system.saturating_sub(prev.system) as f64;
    if sys_delta <= 0.0 || cpu_delta <= 0.0 {
        return 0.0;
    }
    (cpu_delta / sys_delta) * cpus.max(1) as f64 * 100.0
}

fn counters(stats: &bollard::container::CPUStats) -> CpuCounters {
    CpuCounters {
        container: stats.cpu_usage.total_usage,
        system: stats.system_cpu_usage.unwrap_or(0),
    }
}

fn online_cpus(stats: &bollard::container::Stats) -> u64 {
    stats
        .cpu_stats
        .online_cpus
        .filter(|n| *n > 0)
        .or_else(|| {
            stats
                .cpu_stats
                .cpu_usage
                .percpu_usage
                .as_ref()
                .map(|v| v.len() as u64)
        })
        .unwrap_or(1)
}

#[async_trait]
impl DockerSource for BollardDocker {
    async fn containers(&self) -> Result<Vec<ContainerStat>> {
        use bollard::container::{ListContainersOptions, StatsOptions};
        use futures_util::StreamExt;

        let summaries = self
            .docker
            .list_containers(Some(ListContainersOptions::<String> {
                all: true,
                ..Default::default()
            }))
            .await
            .context("listing containers")?;

        // Forget counters of containers that no longer exist.
        self.last_cpu.lock().unwrap().retain(|id, _| {
            summaries
                .iter()
                .any(|s| s.id.as_deref() == Some(id.as_str()))
        });

        let mut out = Vec::with_capacity(summaries.len());
        for summary in summaries {
            let Some(id) = summary.id.clone() else {
                continue;
            };
            let name = summary
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_else(|| id.clone());

            let inspect = self.docker.inspect_container(&id, None).await.ok();
            let (running, health, restart_count) = match &inspect {
                Some(i) => {
                    let state = i.state.as_ref();
                    (
                        state.and_then(|s| s.running).unwrap_or(false),
                        state
                            .and_then(|s| s.health.as_ref())
                            .and_then(|h| h.status)
                            .map(|s| s.to_string()),
                        i.restart_count.unwrap_or(0),
                    )
                }
                None => (summary.state.as_deref() == Some("running"), None, 0),
            };

            let mut stats = self.docker.stats(
                &id,
                Some(StatsOptions {
                    stream: false,
                    one_shot: true,
                }),
            );
            let stat = match stats.next().await {
                Some(Ok(s)) => s,
                _ => continue,
            };

            // Docker's own previous reading when it sent one, else ours.
            let cur = counters(&stat.cpu_stats);
            let prev = Some(counters(&stat.precpu_stats))
                .filter(|p| p.system > 0)
                .or_else(|| self.last_cpu.lock().unwrap().get(&id).copied());
            self.last_cpu.lock().unwrap().insert(id.clone(), cur);
            let cpu_pct = cpu_percent(cur, prev, online_cpus(&stat));

            out.push(ContainerStat {
                name,
                running,
                health,
                cpu_pct,
                mem_bytes: stat.memory_stats.usage.unwrap_or(0),
                mem_limit_bytes: stat.memory_stats.limit.unwrap_or(0),
                restart_count,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_is_measured_between_readings_not_since_boot() {
        let at = |container, system| CpuCounters { container, system };
        // One-shot stats: no previous reading, so no rate yet...
        assert_eq!(cpu_percent(at(500, 10_000), None, 4), 0.0);
        assert_eq!(cpu_percent(at(500, 10_000), Some(at(0, 0)), 4), 0.0);
        // ...then the share of the interval: 50 of 1000 on 4 cores = 20%.
        let pct = cpu_percent(at(550, 11_000), Some(at(500, 10_000)), 4);
        assert!((pct - 20.0).abs() < 1e-9, "{pct}");
        // Counters that went backwards (container restarted) read as idle.
        assert_eq!(cpu_percent(at(10, 12_000), Some(at(500, 11_000)), 4), 0.0);
    }

    #[test]
    fn samples_cover_expected_metrics() {
        let stats = vec![ContainerStat {
            name: "web".into(),
            running: true,
            health: Some("healthy".into()),
            cpu_pct: 12.5,
            mem_bytes: 50_000_000,
            mem_limit_bytes: 100_000_000,
            restart_count: 2,
        }];
        let s = container_samples(&stats, 10);
        let get = |m: &str| s.iter().find(|x| x.metric == m).unwrap().value;
        assert_eq!(get("cpu_pct"), 12.5);
        assert_eq!(get("mem_bytes"), 50_000_000.0);
        assert_eq!(get("mem_pct"), 50.0);
        assert_eq!(get("restarts"), 2.0);
        assert_eq!(get("running"), 1.0);
        assert!(s.iter().all(|x| x.scope == "container" && x.key == "web"));
    }
}
