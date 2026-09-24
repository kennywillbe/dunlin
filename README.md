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
  feed, SVG status and uptime badges. A plain sentence at the top says what
  is wrong right now. Title, accent, logo and extra CSS from the config.
- **Alerts**: per-check failure / recovery / reminder thresholds, incidents
  opened and resolved automatically, maintenance windows (now or planned
  ahead) that mute notifications while open, Telegram, ntfy, Discord, Slack,
  Pushover and webhook notifiers, a daily summary, visitor subscriptions.
- **History**: raw per-minute samples for 7 days and hourly aggregates for
  90 days by default; probe results as long as the hourly aggregates, and at
  least the 90 days the status page shows; charts for host, containers and
  check latency; a Prometheus endpoint with the current values.
- **Web**: read pages are public by default; incidents and maintenance are
  managed on `/manage`, and every write action needs the password (argon2, per-IP login rate limit, CSRF check). `protect_read = true`
  puts the read pages behind the password too. A read-only JSON API, with
  API keys for when reads are protected.
- **Config** is a TOML file that is reloaded on change; an invalid new file is
  logged and the running configuration is kept. `listen`, `data_dir`,
  `db_path` and `docker.socket` need a restart; a change to them is logged.

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
cp dunlin.example.toml dunlin.toml     # set proc_root = "/host/proc" and listen = "0.0.0.0:8080"
docker compose -f docker-compose.example.yml up -d
```

Inside the container, `listen` has to be `0.0.0.0:8080` for the published port
to reach dunlin, and a disk check's `mount` is a path in the container: mount
the host filesystem you want measured (the compose file has a commented line
for `/`) and point `mount` at it.

## Configuration

`dunlin.example.toml` is the reference for every key: listen address, data
directory, proc root, Docker socket, systemd toggle, password hash and
`protect_read`, API keys, notifiers, daily summary, retention, groups,
components and all check types with their intervals and thresholds. `dunlin
check-config --config <path>` validates a file and prints every problem at once.

Durations accept `30s`, `5m`, `1h30m`, `7d` or a bare number of seconds.

### Timezone

`timezone` (top level, an IANA name such as `"Europe/Istanbul"`, default
`"UTC"`) decides where a day starts: the 90-day strips, the "What happened
lately" rows, the month groups on `/incidents` and when the daily summary is
sent. Times of day are still shown in each visitor's own zone by the page
script. Day and month labels are not, they stay in the configured zone.

Maintenance planned on `/manage` starts at a wall time in this zone; the
field's label names it. A time skipped by a DST change starts when the clock
resumes, a repeated one at its first occurrence. An empty start means now, a
start in the past is moved to now and one more than a year out to a year.
A planned window shows on the status page but mutes nothing until it starts.

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

The check fails when no ping arrives within `period + grace`. A check that has
never been pinged gets the same time from when dunlin starts watching it, so a
new nightly job is not reported down before its first night. Tokens are
compared in constant time.

## Notifiers

Each `[[notifiers]]` entry is one channel; `dunlin.example.toml` shows the keys
for every type.

- **ntfy** posts to the server root (`https://ntfy.sh` or your own) with the
  topic in the body; `token` is optional and sent as a bearer token. Priority
  goes from 5 for a down alert to 2 for the summary.
- **Discord** and **Slack** take an incoming webhook URL and get one message
  coloured by state. Mentions such as `@everyone` never ping. A rate-limited
  send is retried once.
- **Pushover** needs an application `token` and a `user` (or group) key. Down
  alerts use priority 1, everything else 0.

With `public_url` set, the incident link becomes the notification's click
target instead of a line in the text.

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
  With `public_url` set, `message` ends with a link to that incident's page.
- `timestamp`: Unix seconds.

## Subscriptions

Visitors can sign up on `/subscribe` to hear about incidents (opened, every
update, resolved, automatic ones included) and, if they tick the box, about
maintenance (planned ahead, started, completed, or called off before it
began). They pick all components or some; an incident on all components goes
to everyone. An automatic incident opened during maintenance is not sent at
all, like the operator alerts it would have caused. Nothing is sent until
they open the confirmation link, which works for 24 hours, and every message
carries a link to unsubscribe.

Subscriptions are off by default and need `public_url`, since the links point
there:

```toml
[subscriptions]
enabled = true
```

Each channel (email, webhook, Telegram) is switched on in its own
`[subscriptions.<channel>]` table; until one is, the page says subscriptions
are not available. Messages go out from a queue in the database, so they
survive a restart; a failed send is retried with growing gaps for about an
hour. The sign-up form is limited to 5 attempts per address an hour and never
says whether an address was already subscribed. With `protect_read = true`
only logged-in visitors can sign up, but confirmation and unsubscribe links
work for anyone. `/manage` lists subscribers with their addresses shortened
and can remove them.

### Email

```toml
[subscriptions.email]
enabled = true
host = "smtp.example.org"
port = 587                    # default: 587 for starttls, 465 for implicit
tls = "starttls"              # or "implicit"
username = "status@example.org"
password = "..."
from = "Acme Status <status@example.org>"
```

Mail is plain text, a few per second at most. Each one has an unsubscribe
link and the `List-Unsubscribe` / `List-Unsubscribe-Post` headers, so mail
clients offer one-click unsubscribe. A mailbox the server reports as unknown
(550, 551, 553) is not retried; other failures, a wrong password included,
are retried with the rest of the queue. Changes to this table apply on reload.

Whether the mail reaches inboxes depends on the sending domain: set up SPF,
DKIM and DMARC for the `from` domain with your mail provider. dunlin only
hands the mail to your SMTP server and cannot do this for you.

## Badges

Each component has badges for READMEs and other dashboards, showing the same
state and 90-day uptime as the status page:

- `/badge/<component>.svg`: current state, e.g. `Website | operational`
- `/badge/<component>/uptime.svg`: 90-day uptime; green from 99.9%, amber
  from 99%, orange from 95%, red below
- `/badge/<component>.json`: the state in the
  [shields.io endpoint](https://shields.io/badges/endpoint-badge) format

```markdown
![Website](https://status.example.org/badge/homepage.svg)
```

`<component>` is the component `id`. With `protect_read = true` badges need an
[API key](#api) (`?token=`) or a logged-in session.

## Prometheus

`/metrics/prometheus` serves the current values in the Prometheus text format.
It is public unless `protect_read = true`, in which case it needs an
[API key](#api) or a logged-in session.

```yaml
scrape_configs:
  - job_name: dunlin
    metrics_path: /metrics/prometheus
    # Only with protect_read = true: a token from `dunlin hash-token`.
    # authorization:
    #   credentials_file: /etc/prometheus/dunlin-token
    static_configs:
      - targets: ["status.example.org:8080"]
```

All metrics are gauges in base units (seconds, bytes, ratios from 0 to 1):

- `dunlin_check_up{check}`, `dunlin_check_latency_seconds{check}`: the latest
  probe; a check with no result yet is left out.
- `dunlin_component_state{component,state}`: 1 for the state the status page
  shows, 0 for the other four.
- `dunlin_incidents_open`
- `dunlin_host_cpu_usage_ratio`, `dunlin_host_memory_used_ratio`,
  `dunlin_host_swap_used_ratio`, `dunlin_host_load1`, `dunlin_host_load5`,
  `dunlin_host_load15`, `dunlin_host_disk_used_ratio{mount}`,
  `dunlin_host_network_receive_bytes_per_second`,
  `dunlin_host_network_transmit_bytes_per_second`
- `dunlin_container_cpu_usage_ratio{container}` (1 is one full core),
  `dunlin_container_memory_bytes{container}`,
  `dunlin_container_running{container}`
- `dunlin_build_info{version}`

Host and container values older than 3 minutes (three collector rounds) are
left out, so a removed container or a stalled collector shows as missing
rather than frozen.

## API

A read-only JSON API under `/api/v1`, sent with `Cache-Control: no-cache`.
States are the snake_case names from the webhook payload and times are Unix
seconds.

- `GET /api/v1/status`: `status` (the worst component state), `generated_at`
  and `components`, each with `id`, `name`, `group`, `state`, `since` (when
  its oldest open incident started, or `null`) and `uptime_90d` (the status
  page's percentage, or `null` without data).
- `GET /api/v1/incidents?limit=N`: newest first, open and resolved; `limit`
  defaults to 20, at most 100. Each has `id`, `title`, `component` (empty for
  all components), `state`, `impact`, `created_at`, `resolved_at` and `auto`.
- `GET /api/v1/incidents/<id>`: one incident plus its `updates` (`state`,
  `message`, `created_at`), newest first.

With `protect_read = false` the API, badges and `/metrics/prometheus` are
public. With `protect_read = true` they need an API key or a logged-in session;
the API answers `401 {"error":"unauthorized"}` otherwise. Make a key with
`dunlin hash-token` and put only the hash in the config:

```toml
[[api_keys]]
name = "grafana"   # shown in logs
hash = "<hash printed by dunlin hash-token>"
```

Send the token as `Authorization: Bearer <token>`, or as `?token=<token>` for
clients that cannot set headers (badge images, some scrapers). A token in the
URL can end up in proxy and access logs, so prefer the header where you can.

```sh
curl -H "Authorization: Bearer $DUNLIN_TOKEN" https://status.example.org/api/v1/status
```

## CLI

```
dunlin [--config <path>]     run the server (default: dunlin.toml)
dunlin hash-password         read a password on stdin, print an argon2 hash
dunlin hash-token            make an API token and print it with its hash
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
