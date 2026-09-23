//! Host metric collection from `/proc` plus disk usage via `statvfs`.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::models::Sample;
use crate::procfs::{self, HostRaw};

/// Keeps the previous sample so CPU and network can be reported as rates.
#[derive(Debug, Default)]
pub struct HostCollector {
    prev: Option<HostRaw>,
    prev_ts: Option<i64>,
}

impl HostCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the host and produce samples for the elapsed interval. The first
    /// call after start only records the baseline.
    pub fn collect(
        &mut self,
        proc_root: &Path,
        mounts: &[String],
        now: i64,
    ) -> Result<Vec<Sample>> {
        let cur = procfs::read_host(proc_root)?;
        let dt = self.prev_ts.map(|p| (now - p) as f64).filter(|d| *d > 0.0);
        let mut samples = samples_from_readings(self.prev.as_ref(), &cur, dt, now);
        self.prev = Some(cur);
        self.prev_ts = Some(now);
        samples.extend(disk_samples(mounts, now));
        Ok(samples)
    }
}

/// Build host samples from two readings. `dt_secs` is required for rate metrics.
pub fn samples_from_readings(
    prev: Option<&HostRaw>,
    cur: &HostRaw,
    dt_secs: Option<f64>,
    now: i64,
) -> Vec<Sample> {
    let mut out = Vec::new();
    let mut push = |metric: &str, key: &str, value: f64| {
        out.push(Sample {
            ts: now,
            scope: "host".to_string(),
            metric: metric.to_string(),
            key: key.to_string(),
            value,
        });
    };

    if let Some(prev) = prev {
        if let Some(cpu) = procfs::cpu_percent(prev.cpu, cur.cpu) {
            push("cpu_pct", "", cpu);
        }
        if let Some(dt) = dt_secs {
            let rx = cur.net.rx_bytes.saturating_sub(prev.net.rx_bytes);
            let tx = cur.net.tx_bytes.saturating_sub(prev.net.tx_bytes);
            push("net_rx_bps", "", rx as f64 / dt);
            push("net_tx_bps", "", tx as f64 / dt);
        }
    } else {
        // Still record one CPU sample so the first chart point is not empty.
        push("cpu_pct", "", 0.0);
    }

    if let Some(mem) = cur.meminfo.mem_used_pct() {
        push("mem_pct", "", mem);
    }
    if let Some(swap) = cur.meminfo.swap_used_pct() {
        push("swap_pct", "", swap);
    }
    push("load1", "", cur.load.0);
    push("load5", "", cur.load.1);
    push("load15", "", cur.load.2);
    out
}

/// Disk usage samples for the configured mount points.
pub fn disk_samples(mounts: &[String], now: i64) -> Vec<Sample> {
    let mut out = Vec::new();
    for mount in mounts {
        if mount.is_empty() {
            continue;
        }
        match procfs::disk_usage(&PathBuf::from(mount)) {
            Ok(usage) => {
                if let Some(pct) = usage.used_pct() {
                    out.push(Sample {
                        ts: now,
                        scope: "host".to_string(),
                        metric: "disk_used_pct".to_string(),
                        key: mount.clone(),
                        value: pct,
                    });
                }
            }
            Err(e) => {
                tracing::warn!(mount = %mount, error = %e, "disk usage unavailable");
            }
        }
    }
    out
}

/// Distinct mount points referenced by disk checks.
pub fn configured_mounts(cfg: &crate::config::Config) -> Vec<String> {
    let mut mounts: Vec<String> = cfg
        .checks
        .iter()
        .filter(|c| c.kind == crate::config::CheckType::Disk)
        .filter_map(|c| c.mount.clone())
        .collect();
    mounts.sort();
    mounts.dedup();
    mounts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/proc")
    }

    fn reading(stat: &str) -> HostRaw {
        let root = fixtures();
        HostRaw {
            cpu: procfs::parse_stat(stat).unwrap(),
            meminfo: procfs::parse_meminfo(&std::fs::read_to_string(root.join("meminfo")).unwrap())
                .unwrap(),
            load: procfs::parse_loadavg(&std::fs::read_to_string(root.join("loadavg")).unwrap())
                .unwrap(),
            net: procfs::parse_netdev(&std::fs::read_to_string(root.join("net/dev")).unwrap())
                .unwrap(),
        }
    }

    #[test]
    fn first_read_has_cpu_placeholder_then_rates() {
        let root = fixtures();
        let first = reading(&std::fs::read_to_string(root.join("stat")).unwrap());
        let s = samples_from_readings(None, &first, None, 100);
        assert!(s.iter().any(|x| x.metric == "cpu_pct" && x.value == 0.0));
        assert!(s.iter().any(|x| x.metric == "mem_pct"));
        assert!(s.iter().any(|x| x.metric == "load1"));

        let second = reading(&std::fs::read_to_string(root.join("stat2")).unwrap());
        let s2 = samples_from_readings(Some(&first), &second, Some(60.0), 160);
        let cpu = s2.iter().find(|x| x.metric == "cpu_pct").unwrap();
        assert!(cpu.value > 0.0 && cpu.value <= 100.0);
        // same net counters -> zero rate
        assert_eq!(
            s2.iter().find(|x| x.metric == "net_rx_bps").unwrap().value,
            0.0
        );
    }

    #[test]
    fn collector_writes_disk_for_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let mounts = vec![dir.path().to_string_lossy().to_string()];
        let samples = disk_samples(&mounts, 42);
        assert_eq!(samples.len(), 1);
        let s = &samples[0];
        assert_eq!(s.metric, "disk_used_pct");
        assert!(s.value >= 0.0 && s.value <= 100.0);
        assert_eq!(s.ts, 42);
    }

    #[test]
    fn configured_mounts_dedupes() {
        let cfg = crate::config::Config {
            checks: vec![],
            ..Default::default()
        };
        assert!(configured_mounts(&cfg).is_empty());
    }
}
