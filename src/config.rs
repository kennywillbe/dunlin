//! Configuration: TOML schema, parsing, validation and reload.

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// Parse a human duration such as `30s`, `5m`, `1h30m`, `500ms`, `7d`.
///
/// A bare integer is interpreted as seconds so old configs stay readable.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    let mut total = Duration::ZERO;
    let mut rest = s;
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if num_end == 0 {
            bail!("duration {s:?} is malformed");
        }
        let value: f64 = rest[..num_end]
            .parse()
            .with_context(|| format!("invalid number in duration {s:?}"))?;
        let unit_end = rest[num_end..]
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .map(|i| num_end + i)
            .unwrap_or(rest.len());
        let unit = &rest[num_end..unit_end];
        let mult = match unit {
            "ms" => Duration::from_millis(1),
            "s" => Duration::from_secs(1),
            "m" => Duration::from_secs(60),
            "h" => Duration::from_secs(3600),
            "d" => Duration::from_secs(86_400),
            other => bail!("unknown duration unit {other:?} in {s:?}"),
        };
        total += mult.mul_f64(value);
        rest = &rest[unit_end..];
    }
    Ok(total)
}

mod duration {
    use super::parse_duration;
    use serde::{Deserialize, Deserializer};
    use std::time::Duration;

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let raw = String::deserialize(d)?;
        parse_duration(&raw).map_err(serde::de::Error::custom)
    }

    pub fn deserialize_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let raw = Option::<String>::deserialize(d)?;
        raw.map(|s| parse_duration(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}

/// `[web]`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// Require the password for read pages too.
    #[serde(default)]
    pub protect_read: bool,
    /// argon2 PHC string for the write password.
    #[serde(default)]
    pub password_hash: Option<String>,
}

/// `[theme]` light / dark / follow the browser.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Auto,
    Light,
    Dark,
}

impl ThemeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeMode::Auto => "auto",
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        }
    }
}

/// `[theme]`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeConfig {
    #[serde(default = "default_title")]
    pub title: String,
    #[serde(default = "default_accent")]
    pub accent: String,
    #[serde(default)]
    pub mode: ThemeMode,
    /// Relative paths are resolved against the config file's directory.
    #[serde(default)]
    pub logo: Option<PathBuf>,
    #[serde(default)]
    pub custom_css: Option<PathBuf>,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            title: default_title(),
            accent: default_accent(),
            mode: ThemeMode::Auto,
            logo: None,
            custom_css: None,
        }
    }
}

fn default_title() -> String {
    "Status".to_string()
}
fn default_accent() -> String {
    crate::theme::DEFAULT_ACCENT.to_string()
}

/// Upper bound for the logo and custom CSS files. They are held in memory and
/// sent with every page, so anything larger is almost certainly a mistake.
pub const THEME_FILE_MAX_BYTES: u64 = 256 * 1024;

/// Logo image read at load time.
#[derive(Debug, Clone)]
pub struct LogoFile {
    pub content_type: &'static str,
    pub bytes: Arc<[u8]>,
}

/// Theme files read at load time, so a missing or oversized file fails the
/// (re)load instead of breaking pages later.
#[derive(Debug, Clone, Default)]
pub struct ThemeFiles {
    pub logo: Option<LogoFile>,
    pub custom_css: Option<Arc<str>>,
    /// Resolved paths, watched for changes alongside the config file.
    pub paths: Vec<PathBuf>,
}

fn logo_content_type(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => return None,
    })
}

fn read_capped(path: &Path, what: &str) -> Result<Vec<u8>> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("theme.{what} {}: cannot read", path.display()))?;
    if meta.len() > THEME_FILE_MAX_BYTES {
        bail!(
            "theme.{what} {} is {} bytes; the limit is {} KB",
            path.display(),
            meta.len(),
            THEME_FILE_MAX_BYTES / 1024
        );
    }
    std::fs::read(path).with_context(|| format!("theme.{what} {}: cannot read", path.display()))
}

/// Read the logo and custom CSS named in `[theme]`, relative to `base`.
pub fn load_theme_files(theme: &ThemeConfig, base: &Path) -> Result<ThemeFiles> {
    let mut files = ThemeFiles::default();
    if let Some(p) = &theme.logo {
        let path = base.join(p);
        let content_type = logo_content_type(&path).ok_or_else(|| {
            anyhow!(
                "theme.logo {} must be a .png, .svg, .jpg or .webp file",
                path.display()
            )
        })?;
        let bytes = read_capped(&path, "logo")?;
        files.logo = Some(LogoFile {
            content_type,
            bytes: bytes.into(),
        });
        files.paths.push(path);
    }
    if let Some(p) = &theme.custom_css {
        let path = base.join(p);
        let bytes = read_capped(&path, "custom_css")?;
        let css = String::from_utf8(bytes)
            .map_err(|_| anyhow!("theme.custom_css {} is not UTF-8", path.display()))?;
        files.custom_css = Some(css.into());
        files.paths.push(path);
    }
    Ok(files)
}

/// `[retention]`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    #[serde(default = "d7")]
    pub raw_days: u32,
    #[serde(default = "d90")]
    pub hourly_days: u32,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            raw_days: 7,
            hourly_days: 90,
        }
    }
}

fn d7() -> u32 {
    7
}
fn d90() -> u32 {
    90
}

/// `[summary]` — daily digest.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryConfig {
    /// Local wall-clock time, `HH:MM`.
    pub time: String,
    /// IANA timezone, e.g. `Europe/Berlin`.
    #[serde(default = "default_tz")]
    pub timezone: String,
}

fn default_tz() -> String {
    "UTC".to_string()
}

/// `[docker]`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Unix socket path; when absent a local default is used.
    #[serde(default)]
    pub socket: Option<PathBuf>,
}

/// `[systemd]`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemdConfig {
    /// On by default on Linux, off elsewhere.
    #[serde(default = "systemd_default_enabled")]
    pub enabled: bool,
}

impl Default for SystemdConfig {
    fn default() -> Self {
        Self {
            enabled: systemd_default_enabled(),
        }
    }
}

fn systemd_default_enabled() -> bool {
    cfg!(target_os = "linux")
}

/// `[[notifiers]]`
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotifierConfig {
    Telegram {
        token: String,
        chat_id: String,
    },
    Webhook {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

/// `[[groups]]`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupConfig {
    pub id: String,
    pub name: String,
}

/// `[[components]]`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub group: Option<String>,
    /// Id of the check this component mirrors; empty creates a purely manual
    /// component (no automated check).
    #[serde(default)]
    pub check: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Supported check kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckType {
    Http,
    Tcp,
    Systemd,
    Docker,
    Disk,
    Ram,
    Swap,
    Load,
    Heartbeat,
}

/// `[[checks]]`. One struct covers every kind; validation enforces the fields
/// each kind needs, which keeps the TOML flat and the errors specific.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckConfig {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: CheckType,
    #[serde(
        default = "default_interval",
        deserialize_with = "duration::deserialize"
    )]
    pub interval: Duration,
    #[serde(default, deserialize_with = "duration::deserialize_opt")]
    pub timeout: Option<Duration>,
    #[serde(default = "default_failures")]
    pub failures_to_open: u32,
    #[serde(default = "default_successes")]
    pub successes_to_resolve: u32,
    #[serde(
        default = "default_reminder",
        deserialize_with = "duration::deserialize"
    )]
    pub reminder_interval: Duration,

    // HTTP
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub expected_status: Option<Vec<u16>>,
    #[serde(default, deserialize_with = "duration::deserialize_opt")]
    pub latency_degraded: Option<Duration>,
    #[serde(default)]
    pub body_contains: Option<String>,
    /// Warn when the TLS certificate has fewer days left than this.
    #[serde(default)]
    pub cert_days_warn: Option<i64>,

    // TCP
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,

    // systemd
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub expected_active: Option<Vec<String>>,

    // docker
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub restart_limit: Option<u64>,

    // resource thresholds
    #[serde(default)]
    pub mount: Option<String>,
    #[serde(default)]
    pub warn: Option<f64>,
    #[serde(default)]
    pub critical: Option<f64>,

    // heartbeat
    #[serde(default, deserialize_with = "duration::deserialize_opt")]
    pub period: Option<Duration>,
    #[serde(default, deserialize_with = "duration::deserialize_opt")]
    pub grace: Option<Duration>,
    #[serde(default)]
    pub token: Option<String>,
}

impl CheckConfig {
    pub fn timeout(&self, default: Duration) -> Duration {
        self.timeout.unwrap_or(default)
    }
}

fn default_interval() -> Duration {
    Duration::from_secs(60)
}
fn default_failures() -> u32 {
    3
}
fn default_successes() -> u32 {
    2
}
fn default_reminder() -> Duration {
    Duration::from_secs(60 * 60)
}

/// Root configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub db_path: Option<PathBuf>,
    #[serde(default = "default_proc_root")]
    pub proc_root: PathBuf,
    #[serde(default)]
    pub trusted_proxy: bool,
    /// Set the `Secure` flag on the session cookie when served over HTTPS.
    #[serde(default)]
    pub secure_cookies: bool,
    #[serde(default)]
    pub web: WebConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    #[serde(default)]
    pub summary: Option<SummaryConfig>,
    #[serde(default)]
    pub docker: DockerConfig,
    #[serde(default)]
    pub systemd: SystemdConfig,
    #[serde(default)]
    pub notifiers: Vec<NotifierConfig>,
    #[serde(default)]
    pub groups: Vec<GroupConfig>,
    #[serde(default)]
    pub components: Vec<ComponentConfig>,
    #[serde(default)]
    pub checks: Vec<CheckConfig>,
    #[serde(default)]
    pub theme: ThemeConfig,
    /// Contents of the files named in `[theme]`; filled by the loaders.
    #[serde(skip)]
    pub theme_files: ThemeFiles,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}
fn default_proc_root() -> PathBuf {
    PathBuf::from("/proc")
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".to_string(),
            data_dir: default_data_dir(),
            db_path: None,
            proc_root: default_proc_root(),
            trusted_proxy: false,
            secure_cookies: false,
            web: WebConfig::default(),
            retention: RetentionConfig::default(),
            summary: None,
            docker: DockerConfig::default(),
            systemd: SystemdConfig::default(),
            notifiers: Vec::new(),
            groups: Vec::new(),
            components: Vec::new(),
            checks: Vec::new(),
            theme: ThemeConfig::default(),
            theme_files: ThemeFiles::default(),
        }
    }
}

impl Config {
    pub fn sqlite_path(&self) -> PathBuf {
        self.db_path
            .clone()
            .unwrap_or_else(|| self.data_dir.join("dunlin.db"))
    }

    pub fn listen_addr(&self) -> Result<SocketAddr> {
        self.listen
            .parse()
            .with_context(|| format!("invalid listen address {:?}", self.listen))
    }

    pub fn check(&self, id: &str) -> Option<&CheckConfig> {
        self.checks.iter().find(|c| c.id == id)
    }

    pub fn component(&self, id: &str) -> Option<&ComponentConfig> {
        self.components.iter().find(|c| c.id == id)
    }
}

/// Parse and validate a config string; theme files resolve against the
/// working directory.
pub fn parse_str(s: &str) -> Result<Config> {
    parse_str_at(s, Path::new("."))
}

/// Parse and validate a config string whose relative theme paths resolve
/// against `base`.
pub fn parse_str_at(s: &str, base: &Path) -> Result<Config> {
    let mut cfg: Config = toml::from_str(s).context("invalid TOML")?;
    validate(&cfg)?;
    cfg.theme_files = load_theme_files(&cfg.theme, base)?;
    Ok(cfg)
}

/// Read and validate a config file.
pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;
    let base = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    parse_str_at(&text, base).with_context(|| format!("config {}", path.display()))
}

/// Collect every validation problem before failing, so a user can fix them in
/// one pass instead of one error per run.
pub fn validate(cfg: &Config) -> Result<()> {
    let mut errs = Vec::new();

    if let Err(e) = cfg.listen_addr() {
        errs.push(e.to_string());
    }
    if cfg.data_dir.as_os_str().is_empty() {
        errs.push("data_dir must not be empty".to_string());
    }
    if cfg.retention.raw_days == 0 {
        errs.push("retention.raw_days must be at least 1".to_string());
    }
    if cfg.retention.hourly_days < cfg.retention.raw_days {
        errs.push("retention.hourly_days must be >= retention.raw_days".to_string());
    }
    if cfg.summary.is_none() && cfg.notifiers.is_empty() {
        // No notifiers is allowed; it just means alerts only appear in the UI.
    }
    if let Some(s) = &cfg.summary {
        if parse_hhmm(&s.time).is_none() {
            errs.push(format!("summary.time {:?} must be HH:MM", s.time));
        }
        if s.timezone.parse::<chrono_tz::Tz>().is_err() {
            errs.push(format!(
                "summary.timezone {:?} is not a valid IANA name",
                s.timezone
            ));
        }
    }

    match &cfg.web.password_hash {
        None => errs.push(
            "web.password_hash is required (run `dunlin hash-password`); write actions need it"
                .to_string(),
        ),
        Some(h) => {
            if argon2::PasswordHash::new(h).is_err() {
                errs.push("web.password_hash is not a valid argon2 PHC string".to_string());
            }
        }
    }

    if let Err(e) = crate::theme::parse_hex(&cfg.theme.accent) {
        errs.push(format!("theme.{e}"));
    }
    let title = cfg.theme.title.trim();
    if title.is_empty() || title.chars().count() > 80 {
        errs.push("theme.title must be 1 to 80 characters".to_string());
    }

    let mut group_ids = HashSet::new();
    for g in &cfg.groups {
        if !group_ids.insert(g.id.clone()) {
            errs.push(format!("duplicate group id {:?}", g.id));
        }
    }

    let mut check_ids = HashSet::new();
    for c in &cfg.checks {
        if !check_ids.insert(c.id.clone()) {
            errs.push(format!("duplicate check id {:?}", c.id));
        }
        validate_check(c, &mut errs);
    }

    let mut comp_ids = HashSet::new();
    for c in &cfg.components {
        if !comp_ids.insert(c.id.clone()) {
            errs.push(format!("duplicate component id {:?}", c.id));
        }
        if let Some(g) = &c.group {
            if !group_ids.contains(g) {
                errs.push(format!(
                    "component {:?} references unknown group {:?}",
                    c.id, g
                ));
            }
        }
        if let Some(chk) = &c.check {
            if !chk.is_empty() && !check_ids.contains(chk) {
                errs.push(format!(
                    "component {:?} references unknown check {:?}",
                    c.id, chk
                ));
            }
        }
    }

    for n in &cfg.notifiers {
        match n {
            NotifierConfig::Telegram { token, chat_id } => {
                if token.trim().is_empty() || chat_id.trim().is_empty() {
                    errs.push("telegram notifier needs token and chat_id".to_string());
                }
            }
            NotifierConfig::Webhook { url, .. } => {
                if url::Url::parse(url).is_err() {
                    errs.push(format!("webhook notifier url {url:?} is not a valid URL"));
                }
            }
        }
    }

    if errs.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "configuration is invalid:\n  - {}",
            errs.join("\n  - ")
        ))
    }
}

fn validate_check(c: &CheckConfig, errs: &mut Vec<String>) {
    let need = |errs: &mut Vec<String>, field: &str| {
        errs.push(format!(
            "check {:?} ({:?}) requires `{}`",
            c.id, c.kind, field
        ));
    };
    if c.interval.is_zero() {
        errs.push(format!("check {:?} interval must be > 0", c.id));
    }
    match c.kind {
        CheckType::Http => match &c.url {
            None => need(errs, "url"),
            Some(u) => {
                if url::Url::parse(u).is_err() {
                    errs.push(format!("check {:?} url {u:?} is invalid", c.id));
                }
            }
        },
        CheckType::Tcp => {
            if c.host.is_none() {
                need(errs, "host");
            }
            if c.port.is_none() {
                need(errs, "port");
            }
        }
        CheckType::Systemd => {
            if c.unit.is_none() {
                need(errs, "unit");
            }
        }
        CheckType::Docker => {
            if c.container.is_none() {
                need(errs, "container");
            }
        }
        CheckType::Disk => {
            if c.mount.is_none() {
                need(errs, "mount");
            }
        }
        CheckType::Ram | CheckType::Swap | CheckType::Load => {}
        CheckType::Heartbeat => {
            if c.period.is_none() {
                need(errs, "period");
            }
            if c.grace.is_none() {
                need(errs, "grace");
            }
            match &c.token {
                None => need(errs, "token"),
                Some(t) if t.len() < 8 => {
                    errs.push(format!("check {:?} heartbeat token is too short", c.id));
                }
                _ => {}
            }
        }
    }
    if let (Some(w), Some(cr)) = (c.warn, c.critical) {
        if cr < w {
            errs.push(format!(
                "check {:?}: critical ({cr}) must be >= warn ({w})",
                c.id
            ));
        }
    }
}

/// Parse `HH:MM` into (hour, minute).
pub fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h < 24 && m < 60 {
        Some((h, m))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172800));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("5").is_ok());
    }

    fn base() -> String {
        r#"
listen = "127.0.0.1:8080"
[web]
password_hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHR2YWx1ZQ$3f8Qm6i2mS2m0m0m0m0m0m0m0m0m0m0m0m0m"
"#
        .to_string()
    }

    #[test]
    fn minimal_config_needs_password() {
        let mut s = base();
        // replace hash with a real one later; invalid hash must be reported
        s = s.replace(
            "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHR2YWx1ZQ$3f8Qm6i2mS2m0m0m0m0m0m0m0m0m0m0m0m0m",
            "not-a-hash",
        );
        let err = parse_str(&s).unwrap_err();
        assert!(err.to_string().contains("argon2"), "{err}");
    }

    #[test]
    fn unknown_fields_rejected() {
        let s = format!("{}\nbogus = 1\n", base_with_real_hash());
        assert!(parse_str(&s).is_err());
    }

    #[test]
    fn component_references_validated() {
        let s = format!(
            r#"{}
[[groups]]
id = "g1"
name = "G"

[[checks]]
id = "c1"
name = "C"
type = "tcp"
host = "127.0.0.1"
port = 80

[[components]]
id = "comp"
name = "Comp"
group = "missing"
check = "nope"
"#,
            base_with_real_hash()
        );
        let err = parse_str(&s).unwrap_err().to_string();
        assert!(err.contains("unknown group"), "{err}");
        assert!(err.contains("unknown check"), "{err}");
    }

    #[test]
    fn http_requires_url() {
        let s = format!(
            "{}\n[[checks]]\nid=\"c\"\nname=\"C\"\ntype=\"http\"\n",
            base_with_real_hash()
        );
        let err = parse_str(&s).unwrap_err().to_string();
        assert!(err.contains("requires `url`"), "{err}");
    }

    #[test]
    fn heartbeat_requires_token() {
        let s = format!(
            "{}\n[[checks]]\nid=\"c\"\nname=\"C\"\ntype=\"heartbeat\"\nperiod=\"1h\"\ngrace=\"5m\"\n",
            base_with_real_hash()
        );
        let err = parse_str(&s).unwrap_err().to_string();
        assert!(err.contains("requires `token`"), "{err}");
    }

    #[test]
    fn hhmm() {
        assert_eq!(parse_hhmm("09:30"), Some((9, 30)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("9:0"), Some((9, 0)));
    }

    #[test]
    fn theme_defaults_and_validation() {
        let cfg = parse_str(&base_with_real_hash()).unwrap();
        assert_eq!(cfg.theme.title, "Status");
        assert_eq!(cfg.theme.accent, "#0f6f73");
        assert_eq!(cfg.theme.mode, ThemeMode::Auto);
        assert!(cfg.theme_files.logo.is_none());

        for bad in ["teal", "#0f6f7", "#0f6f73;}", "#fff"] {
            let s = format!("{}\n[theme]\naccent = \"{bad}\"\n", base_with_real_hash());
            let err = parse_str(&s).unwrap_err().to_string();
            assert!(err.contains("theme.accent"), "{bad}: {err}");
        }
        let s = format!("{}\n[theme]\nmode = \"sepia\"\n", base_with_real_hash());
        assert!(parse_str(&s).is_err());
        let s = format!("{}\n[theme]\ntitle = \"  \"\n", base_with_real_hash());
        assert!(parse_str(&s).is_err());
    }

    #[test]
    fn theme_files_are_read_relative_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("brand.svg"), "<svg/>").unwrap();
        std::fs::write(dir.path().join("extra.css"), "body{}").unwrap();
        let s = format!(
            "{}\n[theme]\nlogo = \"brand.svg\"\ncustom_css = \"extra.css\"\nmode = \"dark\"\n",
            base_with_real_hash()
        );
        let cfg = parse_str_at(&s, dir.path()).unwrap();
        assert_eq!(cfg.theme.mode, ThemeMode::Dark);
        let logo = cfg.theme_files.logo.as_ref().unwrap();
        assert_eq!(logo.content_type, "image/svg+xml");
        assert_eq!(&*logo.bytes, b"<svg/>");
        assert_eq!(cfg.theme_files.custom_css.as_deref(), Some("body{}"));
        assert_eq!(cfg.theme_files.paths.len(), 2);

        // Loading from a file resolves against that file's directory.
        let path = dir.path().join("dunlin.toml");
        std::fs::write(&path, &s).unwrap();
        assert!(load(&path).unwrap().theme_files.custom_css.is_some());

        let big = vec![b'a'; THEME_FILE_MAX_BYTES as usize + 1];
        std::fs::write(dir.path().join("extra.css"), &big).unwrap();
        let err = parse_str_at(&s, dir.path()).unwrap_err().to_string();
        assert!(err.contains("limit"), "{err}");

        let s = format!("{}\n[theme]\nlogo = \"brand.gif\"\n", base_with_real_hash());
        std::fs::write(dir.path().join("brand.gif"), "GIF89a").unwrap();
        assert!(parse_str_at(&s, dir.path()).is_err());
        let s = format!(
            "{}\n[theme]\ncustom_css = \"missing.css\"\n",
            base_with_real_hash()
        );
        assert!(parse_str_at(&s, dir.path()).is_err());
    }

    pub(super) fn base_with_real_hash() -> String {
        // hash for "correcthorsebattery" generated with argon2 defaults
        let hash = crate::auth::hash_password("correcthorsebattery").unwrap();
        format!("listen = \"127.0.0.1:8080\"\n[web]\npassword_hash = \"{hash}\"\n")
    }
}
