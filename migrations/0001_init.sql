-- Raw per-minute metric samples.
CREATE TABLE IF NOT EXISTS samples (
    ts      INTEGER NOT NULL,
    scope   TEXT    NOT NULL,
    metric  TEXT    NOT NULL,
    key     TEXT    NOT NULL DEFAULT '',
    value   REAL    NOT NULL,
    PRIMARY KEY (ts, scope, metric, key)
) WITHOUT ROWID;

-- Hourly aggregates derived from `samples`.
CREATE TABLE IF NOT EXISTS hourly (
    hour    INTEGER NOT NULL,
    scope   TEXT    NOT NULL,
    metric  TEXT    NOT NULL,
    key     TEXT    NOT NULL DEFAULT '',
    min     REAL    NOT NULL,
    avg     REAL    NOT NULL,
    max     REAL    NOT NULL,
    PRIMARY KEY (hour, scope, metric, key)
) WITHOUT ROWID;

-- Individual probe outcomes (state machine + uptime bars).
CREATE TABLE IF NOT EXISTS check_results (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts        INTEGER NOT NULL,
    check_id  TEXT    NOT NULL,
    ok        INTEGER NOT NULL,
    degraded  INTEGER NOT NULL DEFAULT 0,
    latency_ms REAL,
    message   TEXT
);
CREATE INDEX IF NOT EXISTS idx_check_results_check_ts
    ON check_results (check_id, ts);

-- Heartbeat pings, one row per heartbeat check.
CREATE TABLE IF NOT EXISTS heartbeats (
    check_id  TEXT PRIMARY KEY,
    last_ping INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS incidents (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    component   TEXT    NOT NULL DEFAULT '',
    title       TEXT    NOT NULL,
    state       TEXT    NOT NULL,
    impact      TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    resolved_at INTEGER,
    auto        INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_incidents_created ON incidents (created_at);

CREATE TABLE IF NOT EXISTS incident_updates (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    incident_id INTEGER NOT NULL REFERENCES incidents (id) ON DELETE CASCADE,
    ts          INTEGER NOT NULL,
    state       TEXT    NOT NULL,
    message     TEXT    NOT NULL,
    auto        INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_updates_incident ON incident_updates (incident_id, ts);

CREATE TABLE IF NOT EXISTS maintenance (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    component  TEXT    NOT NULL DEFAULT '',
    note       TEXT    NOT NULL DEFAULT '',
    starts_at  INTEGER NOT NULL,
    ends_at    INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_maintenance_window ON maintenance (starts_at, ends_at);

-- Only the hash of a session token is stored.
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
