-- Visitors who asked to hear about incidents and maintenance.
CREATE TABLE IF NOT EXISTS subscribers (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    channel         TEXT    NOT NULL,
    address         TEXT    NOT NULL,
    all_components  INTEGER NOT NULL DEFAULT 1,
    maintenance     INTEGER NOT NULL DEFAULT 0,
    -- pending | active | quarantined
    status          TEXT    NOT NULL DEFAULT 'pending',
    -- Only hashes of tokens are kept; see subscriptions.rs.
    confirm_hash    TEXT,
    confirm_expires INTEGER,
    unsub_hash      TEXT    NOT NULL,
    created_at      INTEGER NOT NULL,
    confirmed_at    INTEGER,
    -- Delivery failures, counted per window, for webhook quarantine.
    failure_window_start INTEGER,
    failures_in_window   INTEGER NOT NULL DEFAULT 0,
    quarantined_at  INTEGER,
    UNIQUE (channel, address)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_subscribers_confirm
    ON subscribers (confirm_hash) WHERE confirm_hash IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_subscribers_unsub ON subscribers (unsub_hash);
CREATE INDEX IF NOT EXISTS idx_subscribers_status ON subscribers (status);

-- Components a subscriber chose when not subscribed to all of them.
CREATE TABLE IF NOT EXISTS subscriber_components (
    subscriber_id INTEGER NOT NULL REFERENCES subscribers (id) ON DELETE CASCADE,
    component_id  TEXT    NOT NULL,
    PRIMARY KEY (subscriber_id, component_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_subscriber_components_component
    ON subscriber_components (component_id);

-- One row per message to one subscriber, sent by the dispatcher task.
CREATE TABLE IF NOT EXISTS outbox (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    subscriber_id   INTEGER NOT NULL REFERENCES subscribers (id) ON DELETE CASCADE,
    -- "confirm" or an event kind such as "incident_opened".
    kind            TEXT    NOT NULL,
    payload         TEXT    NOT NULL,
    created_at      INTEGER NOT NULL,
    next_attempt_at INTEGER NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    sent_at         INTEGER,
    failed_at       INTEGER,
    last_error      TEXT
);
CREATE INDEX IF NOT EXISTS idx_outbox_due
    ON outbox (next_attempt_at) WHERE sent_at IS NULL AND failed_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_outbox_subscriber ON outbox (subscriber_id);

-- Whether subscribers have been told a window was scheduled / started /
-- completed, so each is sent once across restarts, and a window ended
-- before it began is only called off to people who heard it was planned.
ALTER TABLE maintenance ADD COLUMN scheduled_sent INTEGER NOT NULL DEFAULT 0;
ALTER TABLE maintenance ADD COLUMN started_sent INTEGER NOT NULL DEFAULT 0;
ALTER TABLE maintenance ADD COLUMN completed_sent INTEGER NOT NULL DEFAULT 0;
-- Windows from before subscriptions existed have nobody to tell.
UPDATE maintenance SET started_sent = 1 WHERE starts_at <= CAST(strftime('%s', 'now') AS INTEGER);
UPDATE maintenance SET completed_sent = 1 WHERE ends_at <= CAST(strftime('%s', 'now') AS INTEGER);

-- An automatic incident opened during maintenance: subscribers hear nothing
-- about it, from opening to resolution.
ALTER TABLE incidents ADD COLUMN quiet INTEGER NOT NULL DEFAULT 0;
