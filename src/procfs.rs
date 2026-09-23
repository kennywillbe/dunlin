//! Portable `/proc` parsers.
//!
//! Everything takes a root directory so the parsers stay testable on any OS
//! against fixture files, and so a container can point at a mounted
//! `/host/proc`. We deliberately do not use an OS-specific proc crate.

use std::path::Path;

use anyhow::{bail, Context, Result};

/// Read a file below `root`.
fn read(root: &Path, name: &str) -> Result<String> {
    let path = root.join(name);
    std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CpuStat {
    /// Busy time in USER_HZ (total minus idle and iowait).
    pub busy: u64,
    /// Total time in USER_HZ.
    pub total: u64,
}

/// Parse the aggregate `cpu` line of `/proc/stat`.
pub fn parse_stat(content: &str) -> Result<CpuStat> {
    let line = content
        .lines()
        .find(|l| l.starts_with("cpu "))
        .context("no aggregate cpu line in /proc/stat")?;
    let nums: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|v| v.parse::<u64>())
        .collect::<std::result::Result<_, _>>()
        .context("bad cpu fields in /proc/stat")?;
    if nums.len() < 4 {
        bail!("expected at least 4 cpu fields, got {}", nums.len());
    }
    let total: u64 = nums.iter().sum();
    let idle = nums[3];
    let iowait = nums.get(4).copied().unwrap_or(0);
    Ok(CpuStat {
        busy: total.saturating_sub(idle + iowait),
        total,
    })
}

/// Busy percentage between two samples; `None` when no time passed.
pub fn cpu_percent(prev: CpuStat, cur: CpuStat) -> Option<f64> {
    let dt = cur.total.checked_sub(prev.total)?;
    if dt == 0 {
        return None;
    }
    let db = cur.busy.saturating_sub(prev.busy);
    Some((db as f64 / dt as f64) * 100.0)
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MemInfo {
    pub mem_total_kb: u64,
    pub mem_available_kb: u64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
}

impl MemInfo {
    pub fn mem_used_pct(&self) -> Option<f64> {
        if self.mem_total_kb == 0 {
            return None;
        }
        let used = self.mem_total_kb.saturating_sub(self.mem_available_kb);
        Some(used as f64 / self.mem_total_kb as f64 * 100.0)
    }

    pub fn swap_used_pct(&self) -> Option<f64> {
        if self.swap_total_kb == 0 {
            return None;
        }
        let used = self.swap_total_kb.saturating_sub(self.swap_free_kb);
        Some(used as f64 / self.swap_total_kb as f64 * 100.0)
    }
}

/// Parse `/proc/meminfo` (values are kB).
pub fn parse_meminfo(content: &str) -> Result<MemInfo> {
    let mut info = MemInfo::default();
    for line in content.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let value: u64 = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        match key {
            "MemTotal" => info.mem_total_kb = value,
            "MemAvailable" => info.mem_available_kb = value,
            "SwapTotal" => info.swap_total_kb = value,
            "SwapFree" => info.swap_free_kb = value,
            _ => {}
        }
    }
    if info.mem_total_kb == 0 {
        bail!("no MemTotal in meminfo");
    }
    Ok(info)
}

/// Parse the three load averages from `/proc/loadavg`.
pub fn parse_loadavg(content: &str) -> Result<(f64, f64, f64)> {
    let mut it = content.split_whitespace();
    let parse = |v: Option<&str>| -> Result<f64> {
        v.context("missing load field")?
            .parse()
            .context("bad load value")
    };
    Ok((parse(it.next())?, parse(it.next())?, parse(it.next())?))
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NetTotals {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// Sum receive/transmit bytes over physical interfaces, ignoring loopback.
pub fn parse_netdev(content: &str) -> Result<NetTotals> {
    let mut totals = NetTotals::default();
    for line in content.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name == "lo" || name.is_empty() || name.starts_with("face") {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 16 {
            continue;
        }
        let rx: u64 = fields[0].parse().unwrap_or(0);
        let tx: u64 = fields[8].parse().unwrap_or(0);
        totals.rx_bytes += rx;
        totals.tx_bytes += tx;
    }
    Ok(totals)
}

/// A full host read at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostRaw {
    pub cpu: CpuStat,
    pub meminfo: MemInfo,
    pub load: (f64, f64, f64),
    pub net: NetTotals,
}

pub fn read_host(root: &Path) -> Result<HostRaw> {
    Ok(HostRaw {
        cpu: parse_stat(&read(root, "stat")?)?,
        meminfo: parse_meminfo(&read(root, "meminfo")?)?,
        load: parse_loadavg(&read(root, "loadavg")?)?,
        net: parse_netdev(&read(root, "net/dev")?)?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiskUsage {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

impl DiskUsage {
    pub fn used_pct(&self) -> Option<f64> {
        if self.total_bytes == 0 {
            return None;
        }
        let used = self.total_bytes.saturating_sub(self.available_bytes);
        Some(used as f64 / self.total_bytes as f64 * 100.0)
    }
}

#[cfg(unix)]
pub fn disk_usage(path: &Path) -> Result<DiskUsage> {
    let st =
        nix::sys::statvfs::statvfs(path).with_context(|| format!("statvfs {}", path.display()))?;
    let unit = st.fragment_size().max(1) as u64;
    Ok(DiskUsage {
        total_bytes: st.blocks() as u64 * unit,
        available_bytes: st.blocks_available() as u64 * unit,
    })
}

#[cfg(not(unix))]
pub fn disk_usage(_path: &Path) -> Result<DiskUsage> {
    bail!("disk usage is only supported on unix")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fix(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/proc")
            .join(name)
    }

    #[test]
    fn stat_delta() {
        let text = std::fs::read_to_string(fix("stat")).unwrap();
        let cur = parse_stat(&text).unwrap();
        // second fixture sample has more busy time
        let text2 = std::fs::read_to_string(fix("stat2")).unwrap();
        let next = parse_stat(&text2).unwrap();
        let pct = cpu_percent(cur, next).unwrap();
        assert!(pct > 0.0 && pct <= 100.0, "pct={pct}");
        assert_eq!(cpu_percent(cur, cur), None);
    }

    #[test]
    fn meminfo_fields() {
        let info = parse_meminfo(&std::fs::read_to_string(fix("meminfo")).unwrap()).unwrap();
        assert_eq!(info.mem_total_kb, 8_000_000);
        assert_eq!(info.mem_available_kb, 6_000_000);
        assert_eq!(info.swap_total_kb, 2_000_000);
        assert_eq!(info.swap_free_kb, 1_500_000);
        assert!((info.mem_used_pct().unwrap() - 25.0).abs() < 0.01);
        assert!((info.swap_used_pct().unwrap() - 25.0).abs() < 0.01);
    }

    #[test]
    fn loadavg_fields() {
        assert_eq!(
            parse_loadavg(&std::fs::read_to_string(fix("loadavg")).unwrap()).unwrap(),
            (0.5, 0.6, 0.7)
        );
        assert!(parse_loadavg("").is_err());
    }

    #[test]
    fn netdev_sums_without_loopback() {
        let net = parse_netdev(&std::fs::read_to_string(fix("net/dev")).unwrap()).unwrap();
        // eth0 + wlan0, lo excluded
        assert_eq!(net.rx_bytes, 1000 + 2000);
        assert_eq!(net.tx_bytes, 500 + 700);
    }

    #[test]
    fn read_host_from_root() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/proc");
        let host = read_host(&root).unwrap();
        assert_eq!(host.meminfo.mem_total_kb, 8_000_000);
        assert_eq!(host.net.rx_bytes, 3000);
    }
}
