-- Series rows, kept in step with `samples` and pruned to the raw window by the
-- rollup. The metrics picker reads this table instead of `DISTINCT` over
-- `samples`, whose primary key is (ts, scope, metric, key): a scan ordered by
-- (scope, metric, key) cannot use that key, so it degrades into a full scan with
-- a temp sort as the raw rows grow.
CREATE TABLE IF NOT EXISTS series (
    scope   TEXT    NOT NULL,
    metric  TEXT    NOT NULL,
    key     TEXT    NOT NULL DEFAULT '',
    last_ts INTEGER NOT NULL,
    PRIMARY KEY (scope, metric, key)
) WITHOUT ROWID;

-- Backfill from whatever raw samples are still retained, so an existing
-- database does not start with an empty picker.
INSERT OR IGNORE INTO series (scope, metric, key, last_ts)
SELECT scope, metric, key, MAX(ts) FROM samples GROUP BY scope, metric, key;
