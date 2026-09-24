# dunlin

Self-hosted uptime and server monitoring in one Rust binary: checks, alerts,
metrics history and a status page. A single process collects host and container
metrics, probes HTTP/TCP/heartbeat checks, stores a history in SQLite, opens and
resolves incidents, and serves a status page with graphs.

Licensed under MIT OR Apache-2.0.

## Features

- **Checks**: HTTP (status set, latency threshold, body match, TLS certificate
  days left), TCP connect, systemd unit state (Linux D-Bus), Docker container
  (running / health / restart loop), resource thresholds (disk, RAM, swap, load)
  and heartbeat/cron pings.
- **Status page**: component groups, five states (operational, degraded,
  partial outage, major outage, maintenance), overall banner, 90-day uptime
  bars, active incidents and maintenance, 14 days of past incidents, Atom
  feed. A plain sentence at the top says what is wrong right now. Title,
  accent, logo and extra CSS from the config.
- **Alerts**: per-check failure / recovery / reminder thresholds, incidents
  opened and resolved automatically, optional maintenance windows that mute
  notifications, Telegram and webhook notifiers, a daily summary.
- **History**: raw per-minute samples for 7 days and hourly aggregates for
  90 days by default; probe results as long as the hourly aggregates, and at
  least the 90 days the status page shows; charts for host, containers and
  check latency.
- **Web**: read pages are public by default; incidents and maintenance are
  managed on `/manage`, and every write action needs the password (argon2, per-IP login rate limit, CSRF check). `protect_read = true`
  puts the read pages behind the password too.
- **Config** is a TOML file that is reloaded on change; an invalid new file is
  logged and the running configuration is kept.

## Install (native binary + systemd)

```sh
cargo build --release
sudo install -m755 target/release/dunlin /usr/local/bin/dunlin

sudo useradd --system --home /var/lib/dunlin --create-home dunlin

sudo mkdir -p /etc/dunlin
sudo cp dunlin.example.toml /etc/dunlin/dunlin.toml
printf 'choose-a-password' | dunlin hash-password   # paste into [web].password_hash
sudo $EDITOR /etc/dunlin/dunlin.toml

sudo cp dist/dunlin.service /etc/systemd/system/dunlin.service
sudo systemctl daemon-reload
sudo systemctl enable --now dunlin
```

The service unit grants access to the `docker` group (remove the line if you do
not use Docker checks) and keeps `/proc` readable while making everything else
read-only. Point a reverse proxy at the listen address for HTTPS; set
`secure_cookies = true` and, behind a proxy you control, `trusted_proxy = true`.

## Install (Docker)

`docker-compose.example.yml` shows Docker mode: the host `/proc` is mounted at
`/host/proc` (set `proc_root = "/host/proc"` in the config), the Docker socket
and the system D-Bus socket are optional read-only mounts, and the database
lives in a named volume.

```sh
cp dunlin.example.toml dunlin.toml     # set proc_root = "/host/proc"
docker compose -f docker-compose.example.yml up -d
```

## Configuration

`dunlin.example.toml` is the reference for every key: listen address, data
directory, proc root, Docker socket, systemd toggle, password hash and
`protect_read`, notifiers, daily summary, retention, groups, components and all
check types with their intervals and thresholds. `dunlin check-config --config
<path>` validates a file and prints every problem at once.

Durations accept `30s`, `5m`, `1h30m`, `7d` or a bare number of seconds.

### Timezone

`timezone` (top level, an IANA name such as `"Europe/Istanbul"`, default
`"UTC"`) decides where a day starts: the 90-day strips, the "What happened
lately" rows, the month groups on `/incidents` and when the daily summary is
sent. Times of day are still shown in each visitor's own zone by the page
script. Day and month labels are not, they stay in the configured zone.

DST days are 23 or 25 hours long and count as one day each. Per-day uptime is
summed from 15-minute slots, which lines up with every offset in use today
(`+05:30`, `+05:45` included). Hourly metric aggregates stay in UTC hours.

### Theme

The `[theme]` table changes how the pages look, without touching templates:

```toml
[theme]
title = "Acme status"      # header, tab title and feed title
accent = "#17150f"         # #rrggbb only; links, nav underline, buttons
logo = "logo.svg"          # png, svg, jpg or webp, max 256 KB
custom_css = "custom.css"  # loaded after the built-in stylesheet, max 256 KB
```

Paths are relative to the config file. Both files are read when the config
loads and again on every reload, so a broken or oversized file is reported
like any other config error. The built-in colours are CSS custom properties on
`:root` (`--bg`, `--ink`, `--soft`, `--hair`, `--warn`, `--bad`, `--down`,
`--mnt` and so on), which is usually all a custom stylesheet needs to override.
The pages are light only. The fonts (Bricolage Grotesque and Martian Mono,
SIL Open Font License) are built into the binary, so the pages make no
third-party requests.

The config file is watched. When it changes, dunlin reloads it; if the new file
is invalid nothing changes and the error is logged.

## Heartbeats

Create a heartbeat check with a `token`, then ping it when a job runs:

```sh
curl -fsS -X POST https://dunlin.example.org/hb/<token>
```

The check fails when no ping arrives within `period + grace`. Tokens are compared
in constant time.

## Webhook payload

Each webhook notification is an HTTP `POST` with a JSON body. Any headers under
`[notifiers.headers]` are sent as-is.

```json
{
  "event": "down",
  "title": "Homepage is down",
  "message": "unexpected status 500",
  "component": "homepage",
  "state": "major_outage",
  "incident_id": 12,
  "timestamp": 1758700000
}
```

- `event`: `down`, `up`, `reminder` or `summary`.
- `state`: the component state at the time, one of `operational`, `degraded`,
  `partial_outage`, `major_outage`, `maintenance`.
- `incident_id`: the incident the event belongs to, or `null` (daily summary).
- `timestamp`: Unix seconds.

## CLI

```
dunlin [--config <path>]     run the server (default: dunlin.toml)
dunlin hash-password         read a password on stdin, print an argon2 hash
dunlin check-config          validate the config and exit
dunlin --version
```

## Development

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

The HTTP/TCP probes run against local mock servers and the Docker/systemd
clients sit behind traits, so the test suite needs no network, Docker or systemd.
`/proc` is parsed by small built-in parsers against fixture files; only disk
usage uses `statvfs`.

## License

MIT OR Apache-2.0. See `LICENSE-MIT` and `LICENSE-APACHE`.
