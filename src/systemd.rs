//! systemd unit state via D-Bus. The real client is Linux-only; the trait and
//! sample builder are portable so the rest of the code (and its tests) can run
//! anywhere.

use anyhow::Result;
use async_trait::async_trait;

use crate::models::Sample;

#[derive(Debug, Clone, PartialEq)]
pub struct UnitStat {
    pub unit: String,
    pub active_state: String,
}

#[async_trait]
pub trait SystemdSource: Send + Sync {
    async fn unit_states(&self, units: &[String]) -> Result<Vec<UnitStat>>;
}

/// Build chart samples; `active` is 1 for the `active` state, else 0.
pub fn unit_samples(stats: &[UnitStat], now: i64) -> Vec<Sample> {
    stats
        .iter()
        .map(|u| Sample {
            ts: now,
            scope: "systemd".to_string(),
            metric: "active".to_string(),
            key: u.unit.clone(),
            value: if u.active_state == "active" { 1.0 } else { 0.0 },
        })
        .collect()
}

/// Distinct systemd units referenced by systemd checks.
pub fn configured_units(cfg: &crate::config::Config) -> Vec<String> {
    let mut units: Vec<String> = cfg
        .checks
        .iter()
        .filter(|c| c.kind == crate::config::CheckType::Systemd)
        .filter_map(|c| c.unit.clone())
        .collect();
    units.sort();
    units.dedup();
    units
}

#[cfg(target_os = "linux")]
pub struct ZbusSystemd {
    conn: zbus::Connection,
}

#[cfg(target_os = "linux")]
impl ZbusSystemd {
    pub async fn connect() -> Result<Self> {
        let conn = zbus::Connection::system().await?;
        Ok(Self { conn })
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl SystemdSource for ZbusSystemd {
    async fn unit_states(&self, units: &[String]) -> Result<Vec<UnitStat>> {
        use zbus::zvariant::OwnedObjectPath;

        let manager = zbus::Proxy::new(
            &self.conn,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .await?;

        let mut out = Vec::with_capacity(units.len());
        for unit in units {
            let active_state = match manager
                .call::<_, _, OwnedObjectPath>("GetUnit", &(unit.as_str(),))
                .await
            {
                Ok(path) => match zbus::Proxy::new(
                    &self.conn,
                    "org.freedesktop.systemd1",
                    path.as_str(),
                    "org.freedesktop.systemd1.Unit",
                )
                .await
                {
                    Ok(proxy) => proxy
                        .get_property::<String>("ActiveState")
                        .await
                        .unwrap_or_else(|_| "unknown".to_string()),
                    Err(_) => "unknown".to_string(),
                },
                Err(_) => "not-found".to_string(),
            };
            out.push(UnitStat {
                unit: unit.clone(),
                active_state,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeSystemd(Vec<UnitStat>);

    #[async_trait]
    impl SystemdSource for FakeSystemd {
        async fn unit_states(&self, _units: &[String]) -> Result<Vec<UnitStat>> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn samples_and_units() {
        let stats = vec![
            UnitStat {
                unit: "nginx.service".into(),
                active_state: "active".into(),
            },
            UnitStat {
                unit: "db.service".into(),
                active_state: "failed".into(),
            },
        ];
        let s = unit_samples(&stats, 5);
        assert_eq!(s[0].value, 1.0);
        assert_eq!(s[1].value, 0.0);
        assert_eq!(s[0].scope, "systemd");
    }

    #[tokio::test]
    async fn fake_source_is_usable() {
        let fake = FakeSystemd(vec![UnitStat {
            unit: "a".into(),
            active_state: "active".into(),
        }]);
        let out = SystemdSource::unit_states(&fake, &["a".to_string()])
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
    }
}
