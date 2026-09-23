//! Check probes: HTTP, TCP, resource thresholds, Docker, systemd and
//! heartbeats. Network probes are free functions so they can be tested against
//! local mock servers; stateful checks take the already-collected environment.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::{CheckConfig, CheckType};
use crate::docker::ContainerStat;
use crate::systemd::UnitStat;

/// The result of evaluating one check once.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeOutcome {
    /// Passing (the alert machine counts `false` as a failure).
    pub ok: bool,
    /// Passing but in a warning band.
    pub degraded: bool,
    pub latency_ms: Option<f64>,
    pub message: Option<String>,
}

impl ProbeOutcome {
    pub fn up() -> Self {
        Self {
            ok: true,
            degraded: false,
            latency_ms: None,
            message: None,
        }
    }

    pub fn degraded(msg: impl Into<String>) -> Self {
        Self {
            ok: true,
            degraded: true,
            latency_ms: None,
            message: Some(msg.into()),
        }
    }

    pub fn down(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            degraded: false,
            latency_ms: None,
            message: Some(msg.into()),
        }
    }

    pub fn with_latency(mut self, ms: f64) -> Self {
        self.latency_ms = Some(ms);
        self
    }
}

pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        // Redirects are not followed so the configured allowed set sees the
        // status the server actually returned.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("dunlin/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")
}

/// Probe an HTTP endpoint.
pub async fn probe_http(client: &reqwest::Client, check: &CheckConfig) -> ProbeOutcome {
    let url = match check.url.as_deref() {
        Some(u) => u,
        None => return ProbeOutcome::down("no url configured"),
    };
    let timeout = check.timeout(Duration::from_secs(10));
    let started = Instant::now();
    let resp = match client.get(url).timeout(timeout).send().await {
        Ok(r) => r,
        Err(e) => {
            let msg = if e.is_timeout() {
                format!("timeout after {:?}", timeout)
            } else {
                format!("request failed: {e}")
            };
            return ProbeOutcome::down(msg);
        }
    };
    let latency = started.elapsed().as_secs_f64() * 1000.0;
    let status = resp.status().as_u16();
    let allowed = check
        .expected_status
        .clone()
        .unwrap_or_else(|| (200..400).collect());
    if !allowed.contains(&status) {
        return ProbeOutcome::down(format!("unexpected status {status}")).with_latency(latency);
    }

    let mut outcome = ProbeOutcome::up().with_latency(latency);
    if let Some(needle) = &check.body_contains {
        match resp.text().await {
            Ok(body) if body.contains(needle) => {}
            Ok(_) => {
                return ProbeOutcome::down("body did not contain expected text")
                    .with_latency(latency)
            }
            Err(e) => {
                return ProbeOutcome::down(format!("reading body: {e}")).with_latency(latency)
            }
        }
    }

    if let Some(limit) = check.latency_degraded {
        if started.elapsed() > limit {
            outcome =
                ProbeOutcome::degraded(format!("slow: {latency:.0} ms")).with_latency(latency);
        }
    }

    if let Some(warn_days) = check.cert_days_warn {
        match tls_days_left_from_url(url, timeout).await {
            Ok(days) if days < 0 => {
                return ProbeOutcome::down("TLS certificate expired").with_latency(latency)
            }
            Ok(days) if days < warn_days => {
                outcome = ProbeOutcome::degraded(format!("TLS certificate expires in {days} days"))
                    .with_latency(latency);
            }
            Ok(_) => {}
            Err(e) => {
                // A certificate we could not inspect is worth surfacing, but it
                // should not turn a healthy endpoint into a hard failure.
                outcome.message = Some(format!("TLS check failed: {e}"));
            }
        }
    }
    outcome
}

/// TCP connect with timeout.
pub async fn probe_tcp(check: &CheckConfig) -> ProbeOutcome {
    let (host, port) = match (check.host.as_deref(), check.port) {
        (Some(h), Some(p)) => (h, p),
        _ => return ProbeOutcome::down("no host/port configured"),
    };
    let timeout = check.timeout(Duration::from_secs(5));
    let started = Instant::now();
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect((host, port))).await {
        Ok(Ok(_stream)) => {
            ProbeOutcome::up().with_latency(started.elapsed().as_secs_f64() * 1000.0)
        }
        Ok(Err(e)) => ProbeOutcome::down(format!("connect failed: {e}")),
        Err(_) => ProbeOutcome::down(format!("timeout after {timeout:?}")),
    }
}

/// Resource threshold check against the latest collected value.
pub fn probe_resource(check: &CheckConfig, latest: Option<f64>) -> ProbeOutcome {
    let label = match check.kind {
        CheckType::Disk => format!("disk {}", check.mount.clone().unwrap_or_default()),
        CheckType::Ram => "memory".to_string(),
        CheckType::Swap => "swap".to_string(),
        CheckType::Load => "load".to_string(),
        _ => "resource".to_string(),
    };
    let Some(value) = latest else {
        return ProbeOutcome {
            ok: true,
            degraded: false,
            latency_ms: None,
            message: Some("no sample yet".to_string()),
        };
    };
    if let Some(critical) = check.critical {
        if value >= critical {
            return ProbeOutcome::down(format!("{label} at {value:.1} (>= {critical})"));
        }
    }
    if let Some(warn) = check.warn {
        if value >= warn {
            return ProbeOutcome::degraded(format!("{label} at {value:.1} (>= {warn})"));
        }
    }
    ProbeOutcome::up()
}

/// Docker container check: running, health and restart loop.
pub fn probe_docker(check: &CheckConfig, containers: &[ContainerStat]) -> ProbeOutcome {
    let name = check.container.as_deref().unwrap_or_default();
    let Some(c) = containers.iter().find(|c| c.name == name) else {
        return ProbeOutcome::down(format!("container {name} not found"));
    };
    if !c.running {
        return ProbeOutcome::down("container is not running");
    }
    if let Some(health) = &c.health {
        if health == "unhealthy" {
            return ProbeOutcome::down("container is unhealthy");
        }
        if health == "starting" {
            return ProbeOutcome::degraded("container health is starting");
        }
    }
    if let Some(limit) = check.restart_limit {
        if c.restart_count as u64 > limit {
            return ProbeOutcome::down(format!(
                "restart loop: {} restarts (limit {limit})",
                c.restart_count
            ));
        }
    }
    ProbeOutcome::up()
}

/// systemd unit check against the expected ActiveState values.
pub fn probe_systemd(check: &CheckConfig, states: &[UnitStat]) -> ProbeOutcome {
    let unit = check.unit.as_deref().unwrap_or_default();
    let Some(u) = states.iter().find(|u| u.unit == unit) else {
        return ProbeOutcome::down(format!("unit {unit} state unknown"));
    };
    let expected = check
        .expected_active
        .clone()
        .unwrap_or_else(|| vec!["active".to_string()]);
    if expected.iter().any(|e| e == &u.active_state) {
        ProbeOutcome::up()
    } else {
        ProbeOutcome::down(format!("unit {unit} is {}", u.active_state))
    }
}

/// Heartbeat check: late when nothing arrived within `period + grace`.
pub fn probe_heartbeat(check: &CheckConfig, last_ping: Option<i64>, now: i64) -> ProbeOutcome {
    let period = check.period.unwrap_or(Duration::from_secs(3600));
    let grace = check.grace.unwrap_or(Duration::from_secs(300));
    let allowed = period.saturating_add(grace).as_secs() as i64;
    match last_ping {
        None => ProbeOutcome::down("no ping received yet"),
        Some(ts) => {
            let age = now - ts;
            if age > allowed {
                ProbeOutcome::down(format!("last ping {age}s ago (allowed {allowed}s)"))
            } else {
                ProbeOutcome::up()
            }
        }
    }
}

/// Days until `not_after`, negative when already expired.
pub fn days_left(not_after: i64, now: i64) -> i64 {
    (not_after - now).div_euclid(86_400)
}

/// Connect with rustls and return the days left on the peer certificate.
///
/// We use a custom verifier that accepts any chain and records the end-entity
/// certificate, because the goal is to read the expiry from a certificate we
/// may not trust, not to validate the connection. Requiring a trusted chain
/// here would make monitoring private or self-signed endpoints impossible.
pub async fn tls_days_left(host: &str, port: u16, timeout: Duration) -> Result<i64> {
    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(CaptureVerifier {
            captured: captured.clone(),
        }))
        .with_no_client_auth();

    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let server_name = rustls_pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| anyhow::anyhow!("invalid TLS server name {host}: {e}"))?;

    let fut = async {
        let tcp = tokio::net::TcpStream::connect((host, port))
            .await
            .with_context(|| format!("connecting to {host}:{port}"))?;
        connector
            .connect(server_name, tcp)
            .await
            .context("TLS handshake")?;
        Ok::<(), anyhow::Error>(())
    };
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| anyhow::anyhow!("TLS handshake timed out"))??;

    let der = captured
        .lock()
        .unwrap()
        .clone()
        .context("server did not present a certificate")?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der).context("parsing certificate")?;
    let not_after = cert.validity().not_after.timestamp();
    let now = chrono::Utc::now().timestamp();
    Ok(days_left(not_after, now))
}

/// Derive host/port from an `https` URL and inspect its certificate.
pub async fn tls_days_left_from_url(url: &str, timeout: Duration) -> Result<i64> {
    let parsed = url::Url::parse(url).context("parsing url")?;
    if parsed.scheme() != "https" {
        anyhow::bail!("not an https url");
    }
    let host = parsed.host_str().context("url has no host")?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    tls_days_left(host, port, timeout).await
}

#[derive(Debug)]
struct CaptureVerifier {
    captured: Arc<Mutex<Option<Vec<u8>>>>,
}

impl rustls::client::danger::ServerCertVerifier for CaptureVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        *self.captured.lock().unwrap() = Some(end_entity.as_ref().to_vec());
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CheckConfig, CheckType};
    use std::time::Duration;

    fn check(kind: CheckType) -> CheckConfig {
        CheckConfig {
            id: "c".into(),
            name: "c".into(),
            kind,
            interval: Duration::from_secs(60),
            timeout: None,
            failures_to_open: 1,
            successes_to_resolve: 1,
            reminder_interval: Duration::from_secs(3600),
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

    async fn mock_server(body: &'static str, status: u16) -> String {
        use axum::{routing::get, Router};
        let app = Router::new().route(
            "/",
            get(move || async move { (axum::http::StatusCode::from_u16(status).unwrap(), body) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn http_ok_and_body_match() {
        let url = mock_server("hello world", 200).await;
        let client = http_client().unwrap();
        let mut c = check(CheckType::Http);
        c.url = Some(url.clone());
        c.body_contains = Some("hello".to_string());
        let out = probe_http(&client, &c).await;
        assert!(out.ok, "{out:?}");
        assert!(out.latency_ms.is_some());

        c.body_contains = Some("nope".to_string());
        let out = probe_http(&client, &c).await;
        assert!(!out.ok);
    }

    #[tokio::test]
    async fn http_status_set() {
        let url = mock_server("x", 500).await;
        let client = http_client().unwrap();
        let c = CheckConfig {
            url: Some(url),
            ..check(CheckType::Http)
        };
        assert!(!probe_http(&client, &c).await.ok);

        let ok = CheckConfig {
            expected_status: Some(vec![500]),
            ..check(CheckType::Http)
        };
        let ok = CheckConfig {
            url: Some(mock_server("x", 500).await),
            ..ok
        };
        assert!(probe_http(&client, &ok).await.ok);
    }

    #[tokio::test]
    async fn http_latency_threshold_degraded() {
        // A server that never responds quickly; threshold 0 marks it degraded.
        let url = mock_server("x", 200).await;
        let client = http_client().unwrap();
        let c = CheckConfig {
            url: Some(url),
            latency_degraded: Some(Duration::from_millis(0)),
            ..check(CheckType::Http)
        };
        let out = probe_http(&client, &c).await;
        assert!(out.ok && out.degraded, "{out:?}");
    }

    #[tokio::test]
    async fn http_unreachable_is_down() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let c = CheckConfig {
            url: Some("http://127.0.0.1:1/".to_string()),
            ..check(CheckType::Http)
        };
        assert!(!probe_http(&client, &c).await.ok);
    }

    #[tokio::test]
    async fn tcp_open_and_closed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let c = CheckConfig {
            host: Some("127.0.0.1".into()),
            port: Some(addr.port()),
            ..check(CheckType::Tcp)
        };
        assert!(probe_tcp(&c).await.ok);

        // Port 1 is almost always closed.
        let closed = CheckConfig {
            host: Some("127.0.0.1".into()),
            port: Some(1),
            ..check(CheckType::Tcp)
        };
        assert!(!probe_tcp(&closed).await.ok);
    }

    #[test]
    fn resource_thresholds() {
        let c = CheckConfig {
            kind: CheckType::Ram,
            warn: Some(70.0),
            critical: Some(90.0),
            ..check(CheckType::Ram)
        };
        assert!(probe_resource(&c, Some(10.0)).ok);
        assert!(probe_resource(&c, Some(80.0)).degraded);
        assert!(!probe_resource(&c, Some(95.0)).ok);
        assert!(probe_resource(&c, None).ok);
    }

    #[test]
    fn docker_states() {
        let c = CheckConfig {
            container: Some("web".into()),
            restart_limit: Some(3),
            ..check(CheckType::Docker)
        };
        let stat = |running, health, restarts| ContainerStat {
            name: "web".into(),
            running,
            health,
            cpu_pct: 0.0,
            mem_bytes: 0,
            mem_limit_bytes: 0,
            restart_count: restarts,
        };
        assert!(probe_docker(&c, &[stat(true, Some("healthy".into()), 0)]).ok);
        assert!(!probe_docker(&c, &[stat(false, None, 0)]).ok);
        assert!(!probe_docker(&c, &[stat(true, Some("unhealthy".into()), 0)]).ok);
        assert!(!probe_docker(&c, &[stat(true, Some("healthy".into()), 9)]).ok);
        assert!(!probe_docker(&c, &[]).ok);
    }

    #[test]
    fn systemd_states() {
        let c = CheckConfig {
            unit: Some("nginx.service".into()),
            ..check(CheckType::Systemd)
        };
        assert!(
            probe_systemd(
                &c,
                &[UnitStat {
                    unit: "nginx.service".into(),
                    active_state: "active".into()
                }]
            )
            .ok
        );
        assert!(
            !probe_systemd(
                &c,
                &[UnitStat {
                    unit: "nginx.service".into(),
                    active_state: "failed".into()
                }]
            )
            .ok
        );
        assert!(!probe_systemd(&c, &[]).ok);
    }

    #[test]
    fn heartbeat_on_time_and_late() {
        let c = CheckConfig {
            period: Some(Duration::from_secs(3600)),
            grace: Some(Duration::from_secs(300)),
            ..check(CheckType::Heartbeat)
        };
        assert!(!probe_heartbeat(&c, None, 1000).ok);
        assert!(probe_heartbeat(&c, Some(1000), 1000 + 3900).ok);
        assert!(!probe_heartbeat(&c, Some(1000), 1000 + 3901).ok);
    }

    #[test]
    fn days_left_math() {
        assert_eq!(days_left(0, 0), 0);
        assert_eq!(days_left(86_400 * 3, 0), 3);
        assert_eq!(days_left(0, 86_400), -1);
        assert_eq!(days_left(86_400 * 5 + 100, 100), 5);
    }
}
